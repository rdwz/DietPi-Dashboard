use anyhow::Context;
use futures::{SinkExt, StreamExt};
use pty_process::Command;
use std::io::{Read, Write};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tracing::{instrument, Instrument};

use crate::{handle_error, page_handlers, shared, systemdata, CONFIG};

enum TokenState {
    InvalidToken,
    ValidToken,
    NoFingerprint,
}

impl TokenState {
    const fn as_bool(&self) -> bool {
        if let Self::ValidToken = self {
            return true;
        }
        false
    }
}

#[instrument(level = "debug")]
fn validate_token(token: &str, fingerprint: Option<&str>) -> TokenState {
    let mut validator = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::HS256);
    validator.set_issuer(&["DietPi Dashboard"]);
    validator.set_required_spec_claims(&["exp", "iat"]);
    if let Ok(claims) = jsonwebtoken::decode::<shared::JWTClaims>(
        token,
        &jsonwebtoken::DecodingKey::from_secret(CONFIG.secret.as_bytes()),
        &validator,
    ) {
        if let Some(fingerprint) = fingerprint {
            if claims.claims.fingerprint != fingerprint {
                return TokenState::InvalidToken;
            }
        } else {
            return TokenState::NoFingerprint;
        }
    } else {
        tracing::debug!("Invalid token");
        return TokenState::InvalidToken;
    }
    TokenState::ValidToken
}

#[instrument(skip_all)]
pub async fn socket_handler(
    socket: tokio_tungstenite::WebSocketStream<hyper::upgrade::Upgraded>,
    fingerprint: Option<String>,
    _token: String,
) {
    let (mut socket_send, mut socket_recv) = socket.split();
    let (data_send, mut data_recv) = mpsc::channel(1);
    tokio::task::spawn(async move {
        let mut first_message = true;
        let mut req: shared::RequestTypes;
        let mut token = String::new();
        while let Some(Ok(data)) = socket_recv.next().await {
            if data.is_close() {
                break;
            }
            let data_str = if let Message::Text(text) = data {
                text
            } else {
                continue;
            };
            req = handle_error!(
                shared::RequestTypes::try_from(handle_error!(
                    serde_json::from_str::<shared::Request>(&data_str)
                        .with_context(|| format!("Couldn't parse JSON {}", data_str)),
                    continue
                ))
                .context("Invalid request"),
                continue
            );
            tracing::debug!("Got request {:?}", req);
            if CONFIG.pass {
                if let shared::RequestTypes::Token(req_token) = req {
                    token = req_token;
                    continue;
                }
            }
            if CONFIG.pass {
                let validation = validate_token(&token, fingerprint.as_deref());
                if !validation.as_bool() {
                    if !first_message {
                        tracing::debug!("Requesting login");
                        if let Err(err) = data_send.send(None).await {
                            tracing::error!("Internal error: couldn't initiate login: {}", err);
                            break;
                        }
                    }
                    handle_error!(data_send
                        .send(Some(shared::RequestTypes::Page("/login".to_string())))
                        .await
                        .context("Internal error: couldn't send login request"));
                }
                match validation {
                    TokenState::InvalidToken => continue,
                    TokenState::NoFingerprint => return,
                    TokenState::ValidToken => {}
                }
            }
            if let shared::RequestTypes::Page(_) = req {
                // Quit out of handler
                if first_message {
                    tracing::debug!("First message, not sending quit");
                    first_message = false;
                } else if let Err(err) = data_send.send(None).await {
                    tracing::error!("Internal error: couldn't change page: {}", err);
                    break;
                }
            }
            // Send new page/data
            if let Err(err) = data_send.send(Some(req)).await {
                // Manual error handling here, to use tracing::error
                tracing::error!("Internal error: couldn't send request: {}", err);
                break;
            }
        }
    });
    // Send global message (shown on all pages)
    if socket_send
        .send(crate::json_msg!(&systemdata::global().await, return))
        .await
        .is_err()
    {
        return;
    }
    while let Some(Some(message)) = data_recv.recv().await {
        if let shared::RequestTypes::Page(page) = message {
            if match page.as_str() {
                "/" => page_handlers::main_handler(&mut socket_send, &mut data_recv).await,
                "/process" => {
                    page_handlers::process_handler(&mut socket_send, &mut data_recv).await
                }
                "/software" => {
                    page_handlers::software_handler(&mut socket_send, &mut data_recv).await
                }
                "/management" => {
                    page_handlers::management_handler(&mut socket_send, &mut data_recv).await
                }
                "/service" => {
                    page_handlers::service_handler(&mut socket_send, &mut data_recv).await
                }
                "/browser" => {
                    page_handlers::browser_handler(&mut socket_send, &mut data_recv).await
                }
                "/login" => {
                    tracing::debug!("Sending login message");
                    // Internal poll, see other thread
                    if socket_send
                        .send(Message::text("{\"reauth\": true}"))
                        .await
                        .is_err()
                    {
                        break;
                    }
                    false
                }
                _ => {
                    tracing::debug!("Got page {}, not handling", page);
                    false
                }
            } {
                break;
            }
        }
    }
}

#[derive(serde::Deserialize, Debug)]
struct TTYSize {
    cols: u16,
    rows: u16,
}

#[instrument(skip_all)]
pub async fn term_handler(
    socket: tokio_tungstenite::WebSocketStream<hyper::upgrade::Upgraded>,
    fingerprint: Option<String>,
    token: String,
) {
    let (mut socket_send, mut socket_recv) = socket.split();

    if crate::CONFIG.pass && !validate_token(&token, fingerprint.as_deref()).as_bool() {
        return;
    }

    // We use std::process::Command here because it lets us read and write without a mutable reference (even though it should require one?)
    let mut pre_cmd = std::process::Command::new("/bin/login");
    pre_cmd.env("TERM", "xterm");

    let mut cmd = Arc::new(handle_error!(
        if crate::CONFIG.terminal_user == "manual" {
            &mut pre_cmd
        } else {
            pre_cmd.args(&["-f", &crate::CONFIG.terminal_user])
        }
        .spawn_pty(None)
        .context("Couldn't spawn pty"),
        return
    ));
    let mut cmd_read = Arc::clone(&cmd);

    tokio::join!(
        async {
            loop {
                let result = handle_error!(
                    tokio::task::spawn_blocking(move || {
                        let mut data = [0; 256];
                        let res = cmd_read.pty().read(&mut data);
                        (res, data, cmd_read)
                    })
                    .await
                    .context("Couldn't spawn tokio reader thread"),
                    break
                );
                cmd_read = result.2;
                if let Ok(read) = result.0 {
                    if socket_send
                        .send(Message::binary(&result.1[..read]))
                        .await
                        .is_err()
                    {
                        tracing::debug!("Socket closed, breaking");
                        break;
                    }
                } else {
                    tracing::debug!("Terminal closed, breaking");
                    break;
                }
            }
        }
        .instrument(tracing::debug_span!("term_reader")),
        async {
            loop {
                if let Some(Ok(data)) = socket_recv.next().await {
                    match data {
                        Message::Text(data_str) => {
                            if data_str.get(..4) == Some("size") {
                                let json: TTYSize = handle_error!(
                                    serde_json::from_str(&data_str[4..]).with_context(|| format!(
                                        "Couldn't deserialize pty size from {}",
                                        &data_str
                                    )),
                                    continue
                                );
                                tracing::debug!("Got size message {:?}", json);
                                handle_error!(cmd
                                    .resize_pty(&pty_process::Size::new(json.rows, json.cols))
                                    .context("Couldn't resize pty"));
                            } else if cmd.pty().write_all(data_str.as_bytes()).is_err() {
                                tracing::debug!("Terminal closed, breaking");
                                break;
                            }
                        }
                        Message::Binary(data_bin) => {
                            if cmd.pty().write_all(&data_bin).is_err() {
                                tracing::debug!("Terminal closed, breaking");
                                break;
                            }
                        }
                        _ => {}
                    }
                } else {
                    tracing::debug!("Exiting terminal");
                    // Stop bash by writing "exit", since it won't respond to a SIGTERM
                    let _write = cmd.pty().write_all(b"exit\n");
                    break;
                }
            }
        }
        .instrument(tracing::debug_span!("term_writer"))
    );

    // Reap PID, unwrap is safe because all references will have been dropped
    handle_error!(
        handle_error!(
            Arc::get_mut(&mut cmd).context("PTY still has references"),
            return
        )
        .wait()
        .context("Couldn't close terminal"),
        return
    );

    tracing::info!("Closed terminal");
}

#[instrument(level = "debug", skip_all)]
async fn create_zip_file(req: &shared::FileRequest) -> anyhow::Result<Vec<u8>> {
    let src_path = tokio::fs::canonicalize(&req.path)
        .await
        .with_context(|| format!("Invalid source path {}", &req.path))?;
    if src_path.is_dir() {
        tracing::debug!("Source path is directory, recursively walking through");
        let mut zip_file = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for entry in walkdir::WalkDir::new(&src_path) {
            let entry = entry.context("Couldn't get data for recursive entry")?;
            let path = entry.path();
            let name =
                std::path::Path::new(&src_path.file_name().context("Couldn't get file name")?)
                    .join(
                        // Here too, because the path should always be a child
                        path.strip_prefix(&src_path)
                            .context("Path isn't a child of source")?,
                    );
            let name = name.to_string_lossy().to_string();
            if path.is_file() {
                tracing::debug!("Adding file {} to zip", &name);
                let mut file_buf = Vec::new();
                zip_file
                    .start_file(&name, zip::write::FileOptions::default())
                    .with_context(|| format!("Couldn't add file {} to zip", &name))?;
                let mut file = handle_error!(
                    tokio::fs::File::open(path)
                        .await
                        .with_context(|| format!("Couldn't open file {}, skipping", &name)),
                    continue
                );
                handle_error!(
                    file.read_to_end(&mut file_buf)
                        .await
                        .with_context(|| format!("Couldn't read file {}, skipping", &name)),
                    continue
                );
                let tup = tokio::task::spawn_blocking(
                    move || -> (zip::ZipWriter<std::io::Cursor<Vec<u8>>>, Vec<u8>, anyhow::Result<()>) {
                        if let Err(err) = zip_file.write_all(&file_buf) {
                            return (zip_file, file_buf, Err(err.into()));
                        }
                        (zip_file, file_buf, Ok(()))
                    },
                )
                .await
                .context("Couldn't spawn zip task")?;
                zip_file = tup.0;
                file_buf = tup.1;
                handle_error!(tup
                    .2
                    .with_context(|| format!("Couldn't write file {} into zip, skipping", name)));
                file_buf.clear();
            } else if !name.is_empty() {
                tracing::debug!("Adding directory {} to zip", &name);
                zip_file
                    .add_directory(&name, zip::write::FileOptions::default())
                    .with_context(|| format!("Couldn't add directory {} to zip", name))?;
            }
        }
        return Ok(zip_file
            .finish()
            .context("Couldn't finish writing to zip file")?
            .into_inner());
    }
    tracing::debug!("Source path is file, returning file data");
    tokio::fs::read(&src_path)
        .await
        .with_context(|| format!("Couldn't read file {}", src_path.display()))
}

// Not the most elegant solution, but it works
enum FileHandlerHelperReturns {
    String(String),
    SizeBuf(shared::FileSize, Vec<u8>),
    StreamUpload(usize, tokio::fs::File),
}

#[instrument(level = "debug", skip_all)]
async fn file_handler_helper(
    req: &shared::FileRequest,
) -> anyhow::Result<Option<FileHandlerHelperReturns>> {
    tracing::debug!("Command is {}", &req.cmd);
    match req.cmd.as_str() {
        "open" => {
            return Ok(Some(FileHandlerHelperReturns::String(
                tokio::fs::read_to_string(&req.path)
                    .await
                    .with_context(|| format!("Couldn't read file {}", &req.path))?,
            )))
        }
        // Technically works for both files and directories
        "dl" => {
            let buf = create_zip_file(req).await?;
            #[allow(
                clippy::cast_lossless,
                clippy::cast_sign_loss,
                clippy::cast_precision_loss,
                clippy::cast_possible_truncation
            )]
            let size = (buf.len() as f64 / f64::from(1000 * 1000)).ceil() as usize;
            return Ok(Some(FileHandlerHelperReturns::SizeBuf(
                shared::FileSize { size },
                buf,
            )));
        }
        "up" => {
            let file = tokio::fs::File::create(&req.path)
                .await
                .with_context(|| format!("Couldn't create file at {}", &req.path))?;
            return Ok(Some(FileHandlerHelperReturns::StreamUpload(
                req.arg.parse::<usize>().context("Invalid max size")?,
                file,
            )));
        }
        "save" => tokio::fs::write(&req.path, &req.arg)
            .await
            .with_context(|| format!("Couldn't save file {}", &req.path))?,
        _ => tracing::debug!("Got command {}, not handling", &req.cmd),
    }
    Ok(None)
}

fn get_file_req(data: &Message) -> anyhow::Result<shared::FileRequest> {
    if let Message::Text(data_str) = data {
        let req = serde_json::from_str(data_str)
            .with_context(|| format!("Couldn't parse JSON from {}", data_str))?;
        Ok(req)
    } else {
        Err(anyhow::anyhow!(
            "Couldn't convert received data {:?} to text",
            data
        ))
    }
}

#[instrument(skip_all)]
pub async fn file_handler(
    socket: tokio_tungstenite::WebSocketStream<hyper::upgrade::Upgraded>,
    fingerprint: Option<String>,
    token: String,
) {
    let (mut socket_send, mut socket_recv) = socket.split();
    let mut req: shared::FileRequest;

    'outer: while let Some(Ok(data)) = socket_recv.next().await {
        if data.is_close() {
            break;
        }

        req = handle_error!(get_file_req(&data), continue);

        tracing::debug!("Got file request {:?}", req);

        if CONFIG.pass && !validate_token(&token, fingerprint.as_deref()).as_bool() {
            return;
        }

        loop {
            tokio::select! {
                result = file_handler_helper(&req) => {
                    match handle_error!(result, continue) {
                        Some(FileHandlerHelperReturns::String(file)) => {
                            if socket_send.send(Message::text(file)).await.is_err() {
                                break 'outer;
                            }
                        }
                        Some(FileHandlerHelperReturns::SizeBuf(size, buf)) => {
                            if socket_send
                                .send(crate::json_msg!(&size, continue))
                                .await
                                .is_err()
                            {
                                break 'outer;
                            }
                            for i in buf.chunks(1000 * 1000) {
                                if socket_send.feed(Message::binary(i)).await.is_err() {
                                    break 'outer;
                                }
                            }
                            if socket_send.flush().await.is_err() {
                                break 'outer;
                            }
                        }
                        Some(FileHandlerHelperReturns::StreamUpload(size, mut file)) => {
                            while let Some(Ok(Message::Binary(msg))) = (&mut socket_recv).take(size).next().await {
                                handle_error!(file.write_all(&msg).await.with_context(|| {
                                    format!("Couldn't write to file {}, stopping upload", &req.path)
                                }), continue 'outer);
                            }
                        }
                        None => {}
                    }
                    break;
                },
                recv = socket_recv.next() => match recv {
                    Some(Ok(req_tmp)) => req = handle_error!(get_file_req(&req_tmp), continue 'outer),
                    _ => break 'outer,
                },
            }
        }
    }
}
