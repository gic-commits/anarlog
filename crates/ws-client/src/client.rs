use serde::de::DeserializeOwned;

use futures_util::{
    SinkExt, Stream, StreamExt,
    future::{FutureExt, pending},
};
pub use tokio_tungstenite::tungstenite::{ClientRequestBuilder, Utf8Bytes, protocol::Message};

pub use crate::retry::{WebSocketConnectPolicy, WebSocketRetryCallback, WebSocketRetryEvent};

const TRAILING_MESSAGE_GRACE: std::time::Duration = std::time::Duration::from_secs(30);

#[derive(Debug)]
enum ControlCommand {
    Finalize(Option<Message>),
}

struct OutputDropGuard(Option<tokio::sync::oneshot::Sender<()>>);

impl Drop for OutputDropGuard {
    fn drop(&mut self) {
        if let Some(cancel_tx) = self.0.take() {
            tracing::info!("ws_output_drop_guard_fired");
            let _ = cancel_tx.send(());
        }
    }
}

#[derive(Clone)]
struct KeepAliveConfig {
    interval: std::time::Duration,
    message: Message,
}

#[derive(Clone)]
pub struct WebSocketHandle {
    control_tx: tokio::sync::mpsc::UnboundedSender<ControlCommand>,
}

impl WebSocketHandle {
    pub async fn finalize_with_text(&self, text: Utf8Bytes) {
        let _ = self
            .control_tx
            .send(ControlCommand::Finalize(Some(Message::Text(text))));
    }
}

pub trait WebSocketIO: Send + 'static {
    type Data: Send;
    type Input: Send;
    type Output: DeserializeOwned;

    fn to_input(data: Self::Data) -> Self::Input;
    fn to_message(input: Self::Input) -> Message;
    fn from_message(msg: Message) -> Result<Option<Self::Output>, crate::Error>;
}

pub struct WebSocketClient {
    request: ClientRequestBuilder,
    keep_alive: Option<KeepAliveConfig>,
    connect_policy: WebSocketConnectPolicy,
    on_retry: Option<WebSocketRetryCallback>,
}

impl WebSocketClient {
    pub fn new(request: ClientRequestBuilder) -> Self {
        Self {
            request,
            keep_alive: None,
            connect_policy: WebSocketConnectPolicy::default(),
            on_retry: None,
        }
    }

    pub fn with_keep_alive_message(
        mut self,
        interval: std::time::Duration,
        message: Message,
    ) -> Self {
        self.keep_alive = Some(KeepAliveConfig { interval, message });
        self
    }

    pub fn with_connect_policy(mut self, policy: WebSocketConnectPolicy) -> Self {
        self.connect_policy = policy;
        self
    }

    pub fn on_retry(mut self, callback: WebSocketRetryCallback) -> Self {
        self.on_retry = Some(callback);
        self
    }

    pub async fn from_audio<T: WebSocketIO, S: Stream<Item = T::Data> + Send + Unpin + 'static>(
        &self,
        initial_message: Option<Message>,
        mut audio_stream: S,
    ) -> Result<
        (
            impl Stream<Item = Result<T::Output, crate::Error>> + use<T, S>,
            WebSocketHandle,
        ),
        crate::Error,
    > {
        tracing::info!("ws_client_version=v2_no_trailing_grace");

        let keep_alive_config = self.keep_alive.clone();
        let ws_stream = crate::retry::connect_with_retry(
            self.request.clone(),
            &self.connect_policy,
            self.on_retry.as_ref(),
        )
        .await?;

        let (mut ws_sender, mut ws_receiver) = ws_stream.split();

        let (control_tx, mut control_rx) = tokio::sync::mpsc::unbounded_channel();
        let (error_tx, mut error_rx) = tokio::sync::mpsc::unbounded_channel::<crate::Error>();
        let (cancel_tx, mut cancel_rx) = tokio::sync::oneshot::channel();
        let handle = WebSocketHandle { control_tx };

        let _send_task = tokio::spawn(async move {
            #[derive(Debug)]
            enum SendLoopExit {
                Finalize,
                InputEnded,
                Error,
                Cancelled,
            }

            if let Some(msg) = initial_message
                && let Err(e) = ws_sender.send(msg).await
            {
                tracing::error!("ws_initial_message_failed: {:?}", e);
                let _ = error_tx.send(e.into());
                return;
            }

            let mut last_outbound_at = tokio::time::Instant::now();
            let mut audio_closed = false;
            let mut control_closed = false;

            let exit_reason = loop {
                if audio_closed && control_closed {
                    break SendLoopExit::InputEnded;
                }

                let mut keep_alive_fut = if !audio_closed {
                    if let Some(cfg) = keep_alive_config.as_ref() {
                        tokio::time::sleep_until(last_outbound_at + cfg.interval).boxed()
                    } else {
                        pending().boxed()
                    }
                } else {
                    pending().boxed()
                };

                tokio::select! {
                    biased;

                    _ = &mut cancel_rx => break SendLoopExit::Cancelled,
                    _ = keep_alive_fut.as_mut() => {
                        if let Some(cfg) = keep_alive_config.as_ref() {
                            if let Err(e) = ws_sender.send(cfg.message.clone()).await {
                                tracing::error!("ws_keepalive_failed: {:?}", e);
                                let _ = error_tx.send(e.into());
                                break SendLoopExit::Error;
                            }
                            last_outbound_at = tokio::time::Instant::now();
                        }
                    }
                    maybe_data = audio_stream.next(), if !audio_closed => {
                        match maybe_data {
                            Some(data) => {
                                let input = T::to_input(data);
                                let msg = T::to_message(input);

                                if let Err(e) = ws_sender.send(msg).await {
                                    tracing::error!("ws_send_failed: {:?}", e);
                                    let _ = error_tx.send(e.into());
                                    break SendLoopExit::Error;
                                }
                                last_outbound_at = tokio::time::Instant::now();
                            }
                            None => {
                                audio_closed = true;
                            }
                        }
                    }
                    command = control_rx.recv(), if !control_closed => {
                        match command {
                            Some(ControlCommand::Finalize(maybe_msg)) => {
                                if let Some(msg) = maybe_msg
                                    && let Err(e) = ws_sender.send(msg).await {
                                        tracing::error!("ws_finalize_failed: {:?}", e);
                                        let _ = error_tx.send(e.into());
                                    }
                                break SendLoopExit::Finalize;
                            }
                            None => {
                                control_closed = true;
                            }
                        }
                    }
                    else => break SendLoopExit::InputEnded,
                }
            };

            tracing::info!(
                exit_reason = ?exit_reason,
                audio_closed,
                control_closed,
                "ws_send_loop_exit"
            );

            if matches!(exit_reason, SendLoopExit::Finalize) {
                tokio::select! {
                    _ = tokio::time::sleep(TRAILING_MESSAGE_GRACE) => {}
                    _ = &mut cancel_rx => {}
                }
            }

            tracing::info!("ws_sender_close_calling");
            let _ = ws_sender.close().await;
        });

        let output_stream = async_stream::stream! {
            let _drop_guard = OutputDropGuard(Some(cancel_tx));

            loop {
                tokio::select! {
                    biased;

                    Some(msg_result) = ws_receiver.next() => {
                        match msg_result {
                            Ok(msg) => {
                                match msg {
                                    Message::Text(text) => {
                                        tracing::info!("[DEBUG] WS received text: {}", text);
                                        match T::from_message(Message::Text(text)) {
                                            Ok(Some(output)) => {
                                                yield Ok(output);
                                            }
                                            Ok(None) => {
                                                tracing::info!("[DEBUG] WS text dropped by adapter");
                                            }
                                            Err(error) => {
                                                yield Err(error);
                                                break;
                                            }
                                        }
                                    }
                                    Message::Binary(data) => {
                                        tracing::info!("[DEBUG] WS received binary: {} bytes", data.len());
                                        match T::from_message(Message::Binary(data)) {
                                            Ok(Some(output)) => {
                                                yield Ok(output);
                                            }
                                            Ok(None) => {
                                                tracing::info!("[DEBUG] WS binary dropped by adapter");
                                            }
                                            Err(error) => {
                                                yield Err(error);
                                                break;
                                            }
                                        }
                                    }
                                    Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => continue,
                                    Message::Close(frame) => {
                                        let close_code = frame.as_ref().map(|f| u16::from(f.code));
                                        let close_reason = frame.as_ref().map(|f| f.reason.to_string());
                                        tracing::info!(
                                            close_code = ?close_code,
                                            close_reason = ?close_reason,
                                            "ws_received_close_frame"
                                        );

                                        if let Ok(error) = error_rx.try_recv() {
                                            yield Err(error);
                                            break;
                                        }

                                        if let Some(frame) = frame
                                            && frame.code != tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode::Normal
                                        {
                                            yield Err(crate::Error::remote_closed(
                                                Some(u16::from(frame.code)),
                                                frame.reason.to_string(),
                                            ));
                                        }

                                        break;
                                    }
                                }
                            }
                            Err(e) => {
                                tracing::error!("ws_receiver_failed: {:?}", e);
                                yield Err(e.into());
                                break;
                            }
                        }
                    }
                    Some(error) = error_rx.recv() => {
                        yield Err(error);
                        break;
                    }
                    else => {
                        if let Ok(error) = error_rx.try_recv() {
                            yield Err(error);
                        }
                        break;
                    }
                }
            }
        };

        Ok((output_stream, handle))
    }
}
