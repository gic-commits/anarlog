mod pipeline;
mod stream;

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc::{self, Receiver},
};

use ractor::{Actor, ActorName, ActorProcessingErr, ActorRef, RpcReplyPort};
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use crate::{
    ListenerRuntime, SessionErrorEvent, SessionProgressEvent,
    actors::session::session_span,
    actors::{ChannelMode, ListenerMsg, RecMsg},
};
use hypr_audio::{AudioProvider, CaptureFrame};

use pipeline::Pipeline;
use stream::start_source_loop;

use hypr_device_monitor::{DeviceMonitorHandle, DeviceSwitch, DeviceSwitchMonitor};

pub enum SourceMsg {
    SetMicMute(bool),
    GetMicMute(RpcReplyPort<bool>),
    GetMicDevice(RpcReplyPort<Option<String>>),
    SetMicDevice(Option<String>),
    PrepareListenerRefresh(RpcReplyPort<ListenerRefreshReplay>),
    SetListenerRouting(ListenerRouting),
    SetRecorder(Option<ActorRef<RecMsg>>),
    Frame(SourceFrame),
    StreamFailed(String),
}

pub struct SourceFrame {
    pub capture: CaptureFrame,
    pub mic_muted: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ListenerRefreshReplay {
    pub duration_secs: f64,
}

pub type PcmSender = tokio::sync::mpsc::Sender<Arc<[f32]>>;

#[derive(Clone)]
pub enum ListenerRouting {
    Buffering,
    Attached(ActorRef<ListenerMsg>),
    Dropped,
    ProgressiveBatch(PcmSender),
}

pub struct SourceArgs {
    pub mic_device: Option<String>,
    pub onboarding: bool,
    pub runtime: Arc<dyn ListenerRuntime>,
    pub audio: Arc<dyn AudioProvider>,
    pub session_id: String,
    pub listener_routing: ListenerRouting,
    pub recorder: Option<ActorRef<RecMsg>>,
}

pub struct SourceState {
    pub(super) runtime: Arc<dyn ListenerRuntime>,
    pub(super) audio: Arc<dyn AudioProvider>,
    pub(super) session_id: String,
    pub(super) mic_device: Option<String>,
    pub(super) onboarding: bool,
    pub(super) mic_muted: Arc<AtomicBool>,
    /// True once the user explicitly picked an input device for this process.
    /// While set, default-input changes must NOT restart the source.
    explicit_mic_device: Arc<AtomicBool>,
    pub(super) run_task: Option<tokio::task::JoinHandle<()>>,
    pub(super) stream_cancel_token: Option<CancellationToken>,
    pub(super) current_mode: ChannelMode,
    pub(super) pipeline: Pipeline,
    pub(super) listener_routing: ListenerRouting,
    pub(super) recorder: Option<ActorRef<RecMsg>>,
    _device_watcher: Option<DeviceChangeWatcher>,
    _silence_stream_tx: Option<std::sync::mpsc::Sender<()>>,
}

pub struct SourceActor;

struct DeviceChangeWatcher {
    _handle: DeviceMonitorHandle,
    _thread: std::thread::JoinHandle<()>,
}

impl DeviceChangeWatcher {
    fn spawn(actor: ActorRef<SourceMsg>, follow_default_input: Arc<AtomicBool>) -> Self {
        let (event_tx, event_rx) = mpsc::channel();
        let handle = DeviceSwitchMonitor::spawn_debounced(event_tx);
        let thread =
            std::thread::spawn(move || Self::event_loop(event_rx, actor, follow_default_input));

        Self {
            _handle: handle,
            _thread: thread,
        }
    }

    fn event_loop(
        event_rx: Receiver<DeviceSwitch>,
        actor: ActorRef<SourceMsg>,
        follow_default_input: Arc<AtomicBool>,
    ) {
        loop {
            match event_rx.recv() {
                Ok(DeviceSwitch::DefaultInputChanged) => {
                    // Only follow OS default-input switches while the user has
                    // not explicitly pinned an input device for this run.
                    if follow_default_input.load(Ordering::Relaxed) {
                        tracing::info!("default_input_changed_restarting_source");
                        actor.stop(Some("device_change".to_string()));
                    } else {
                        tracing::info!("default_input_changed_ignored_explicit_device");
                    }
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
    }
}

impl SourceActor {
    pub fn name() -> ActorName {
        "source".into()
    }
}

#[ractor::async_trait]
impl Actor for SourceActor {
    type Msg = SourceMsg;
    type State = SourceState;
    type Arguments = SourceArgs;

    async fn pre_start(
        &self,
        myself: ActorRef<Self::Msg>,
        args: Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        let session_id = args.session_id.clone();
        let span = session_span(&session_id);

        async {
            args.runtime
                .emit_progress(SessionProgressEvent::AudioInitializing {
                    session_id: session_id.clone(),
                });

            let silence_stream_tx = Some(args.audio.play_silence());
            let explicit_mic_device = Arc::new(AtomicBool::new(args.mic_device.is_some()));
            let mic_device = args
                .mic_device
                .or_else(|| Some(args.audio.default_device_name()));
            tracing::info!(mic_device = ?mic_device);

            let pipeline = Pipeline::new(args.runtime.clone(), args.session_id.clone());

            let device_watcher =
                DeviceChangeWatcher::spawn(myself.clone(), explicit_mic_device.clone());

            let mut st = SourceState {
                runtime: args.runtime,
                audio: args.audio,
                session_id: args.session_id,
                mic_device,
                onboarding: args.onboarding,
                mic_muted: Arc::new(AtomicBool::new(false)),
                explicit_mic_device,
                run_task: None,
                stream_cancel_token: None,
                current_mode: ChannelMode::MicAndSpeaker,
                pipeline,
                listener_routing: args.listener_routing,
                recorder: args.recorder,
                _device_watcher: Some(device_watcher),
                _silence_stream_tx: silence_stream_tx,
            };

            start_source_loop(&myself, &mut st).await?;
            Ok(st)
        }
        .instrument(span)
        .await
    }

    async fn handle(
        &self,
        myself: ActorRef<Self::Msg>,
        msg: Self::Msg,
        st: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        let span = session_span(&st.session_id);
        let _guard = span.enter();

        match msg {
            SourceMsg::SetMicMute(muted) => {
                st.mic_muted.store(muted, Ordering::Relaxed);
            }
            SourceMsg::GetMicMute(reply) => {
                if !reply.is_closed() {
                    let _ = reply.send(st.mic_muted.load(Ordering::Relaxed));
                }
            }
            SourceMsg::GetMicDevice(reply) => {
                if !reply.is_closed() {
                    let _ = reply.send(st.mic_device.clone());
                }
            }
            SourceMsg::SetMicDevice(device) => {
                tracing::info!(?device, "switching_input_device");
                st.mic_device = device.clone();
                st.explicit_mic_device
                    .store(device.is_some(), Ordering::Relaxed);

                // Restart the capture stream with the new device. The session,
                // listener routing and pipeline are untouched — only the raw
                // PCM source is swapped, so the timeline stays continuous.
                if let Some(token) = st.stream_cancel_token.take() {
                    token.cancel();
                }
                if let Some(handle) = st.run_task.take() {
                    if handle.await.is_err() {
                        tracing::warn!("source_stream_task_join_failed");
                    }
                }

                start_source_loop(&myself, st).await?;
            }
            SourceMsg::PrepareListenerRefresh(reply) => {
                st.listener_routing = ListenerRouting::Buffering;
                let replay = st.pipeline.prepare_listener_refresh();
                if !reply.is_closed() {
                    let _ = reply.send(replay);
                }
            }
            SourceMsg::SetListenerRouting(routing) => {
                st.listener_routing = routing;
                st.pipeline
                    .on_listener_routing_changed(&st.listener_routing);
            }
            SourceMsg::SetRecorder(recorder) => {
                st.recorder = recorder;
            }
            SourceMsg::Frame(frame) => {
                st.pipeline.dispatch_frame(
                    frame,
                    st.current_mode,
                    &st.listener_routing,
                    st.recorder.as_ref(),
                );
            }
            SourceMsg::StreamFailed(reason) => {
                tracing::error!(%reason, "source_stream_failed_stopping");
                st.runtime.emit_error(SessionErrorEvent::AudioError {
                    session_id: st.session_id.clone(),
                    error: reason.clone(),
                    device: st.mic_device.clone(),
                    is_fatal: true,
                });
                myself.stop(Some(reason));
            }
        }

        Ok(())
    }

    async fn post_stop(
        &self,
        _myself: ActorRef<Self::Msg>,
        st: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        if let Some(cancel_token) = st.stream_cancel_token.take() {
            cancel_token.cancel();
        }
        if let Some(task) = st.run_task.take() {
            task.abort();
        }

        Ok(())
    }
}
