//! 音频业务状态；载入与 PCM 播放交给 SDK 后台任务，不在 UI 线程 join 或等待设备。
use liteos_ui::{Background, Clock};
use sound::{Sound, Transport};
use std::{
    cell::RefCell,
    future::{Future, poll_fn},
    io,
    path::PathBuf,
    pin::Pin,
    rc::Rc,
    sync::Arc,
    task::{Poll, Waker},
    time::Duration,
};
pub struct Loaded {
    pub name: String,
    pub path: PathBuf,
    pub sound: Arc<Sound>,
    pub peaks: Vec<f32>,
}
impl Loaded {
    pub fn load(path: PathBuf) -> io::Result<Self> {
        let sound = Sound::load(&path)?;
        let peaks = sound.peaks(512);
        let name = path.file_name().map_or_else(
            || path.display().to_string(),
            |name| name.to_string_lossy().into_owned(),
        );
        Ok(Self {
            name,
            path,
            sound: Arc::new(sound),
            peaks,
        })
    }
}
enum Phase {
    Idle,
    Running(Arc<Transport>),
    Draining {
        transport: Arc<Transport>,
        resume: Option<usize>,
    },
}
pub struct Job {
    sound: Arc<Sound>,
    transport: Arc<Transport>,
    start: usize,
}
pub struct Player {
    pub loaded: Option<Loaded>,
    phase: Phase,
    cursor: usize,
    pub message: String,
    pub loading: bool,
    pub closing: bool,
    revision: u64,
    changed: Option<Waker>,
    stopped: Option<Waker>,
}
impl Player {
    pub fn new() -> Self {
        Self {
            loaded: None,
            phase: Phase::Idle,
            cursor: 0,
            message: String::new(),
            loading: false,
            closing: false,
            revision: 0,
            changed: None,
            stopped: None,
        }
    }
    pub fn changed(&mut self) {
        self.revision = self.revision.wrapping_add(1);
        // SDK Waker 只排队，不在借用或锁内执行 UI；两个等待者分别拥有刷新与关闭通知。
        if let Some(wake) = self.changed.take() {
            wake.wake();
        }
        if self.idle()
            && let Some(wake) = self.stopped.take()
        {
            wake.wake();
        }
    }
    pub fn frames(&self) -> usize {
        self.loaded
            .as_ref()
            .map_or(0, |loaded| loaded.sound.frames())
    }
    pub fn idle(&self) -> bool {
        matches!(self.phase, Phase::Idle)
    }
    pub fn playing(&self) -> bool {
        matches!(
            self.phase,
            Phase::Running(_)
                | Phase::Draining {
                    resume: Some(_),
                    ..
                }
        )
    }
    pub fn position(&self) -> usize {
        match &self.phase {
            Phase::Running(transport) => transport.position().min(self.frames()),
            Phase::Draining {
                resume: Some(position),
                ..
            } => *position,
            _ => self.cursor,
        }
    }
    pub fn stop(&mut self) {
        let phase = std::mem::replace(&mut self.phase, Phase::Idle);
        self.phase = match phase {
            Phase::Running(transport) => {
                self.cursor = transport.position().min(self.frames());
                transport.stop();
                Phase::Draining {
                    transport,
                    resume: None,
                }
            }
            Phase::Draining { transport, .. } => {
                transport.stop();
                Phase::Draining {
                    transport,
                    resume: None,
                }
            }
            Phase::Idle => Phase::Idle,
        };
        self.changed();
    }
    pub fn seek(&mut self, fraction: i64) {
        let position = (self.frames() as u128 * fraction.clamp(0, 1000) as u128 / 1000) as usize;
        let playing = self.playing();
        self.stop();
        self.cursor = position;
        if let Phase::Draining { resume, .. } = &mut self.phase
            && playing
        {
            *resume = Some(position);
        }
        self.changed();
    }
    pub fn play(&mut self) -> Option<Job> {
        if self.loading || self.closing {
            return None;
        }
        match &mut self.phase {
            Phase::Running(_) => return None,
            Phase::Draining { resume, .. } => {
                *resume = Some(self.cursor);
                self.changed();
                return None;
            }
            Phase::Idle => {}
        }
        let loaded = self.loaded.as_ref()?;
        if self.cursor >= loaded.sound.frames() {
            self.cursor = 0;
        }
        let transport = Arc::new(Transport::default());
        let job = Job {
            sound: loaded.sound.clone(),
            transport: transport.clone(),
            start: self.cursor,
        };
        self.phase = Phase::Running(transport);
        self.message.clear();
        self.changed();
        Some(job)
    }
    fn completed(&mut self, result: io::Result<()>) -> Option<Job> {
        let phase = std::mem::replace(&mut self.phase, Phase::Idle);
        let resume = match phase {
            Phase::Running(_) => {
                self.cursor = 0;
                false
            }
            Phase::Draining { resume, .. } => {
                if let Some(position) = resume {
                    self.cursor = position;
                    true
                } else {
                    false
                }
            }
            Phase::Idle => false,
        };
        if let Err(error) = result {
            self.message = format!("无法播放：{error}");
            self.changed();
            return None;
        }
        self.changed();
        if resume { self.play() } else { None }
    }
    pub fn begin_load(&mut self) {
        self.stop();
        self.loading = true;
        self.message = "正在载入声音…".into();
        self.changed();
    }
    pub fn loaded(&mut self, result: io::Result<Loaded>) {
        self.loading = false;
        if self.closing {
            return;
        }
        match result {
            Ok(loaded) => {
                self.loaded = Some(loaded);
                self.cursor = 0;
                self.message.clear();
            }
            Err(error) => self.message = format!("无法打开声音：{error}"),
        }
        self.changed();
    }
    pub async fn wait_changed(
        state: &Rc<RefCell<Self>>,
        clock: &Clock,
        seen: u64,
    ) -> liteos_ui::Result<()> {
        let playing = state.borrow().playing();
        let mut timer = if playing {
            Some(clock.sleep(Duration::from_millis(100))?)
        } else {
            None
        };
        poll_fn(|cx| {
            let mut state = state.borrow_mut();
            if state.revision != seen {
                return Poll::Ready(());
            }
            if timer
                .as_mut()
                .is_some_and(|timer| Pin::new(timer).poll(cx).is_ready())
            {
                return Poll::Ready(());
            }
            state.changed = Some(cx.waker().clone());
            Poll::Pending
        })
        .await;
        Ok(())
    }
    pub fn revision(&self) -> u64 {
        self.revision
    }
    pub async fn shutdown(state: &Rc<RefCell<Self>>) {
        {
            let mut state = state.borrow_mut();
            state.closing = true;
            state.stop();
        }
        poll_fn(|cx| {
            let mut state = state.borrow_mut();
            if state.idle() {
                Poll::Ready(())
            } else {
                state.stopped = Some(cx.waker().clone());
                Poll::Pending
            }
        })
        .await
    }
}
struct StopOnDrop(Arc<Transport>);
impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.stop();
    }
}
pub async fn drive(state: Rc<RefCell<Player>>, background: Background, mut job: Job) {
    let mut cleanup = StopOnDrop(job.transport.clone());
    loop {
        cleanup.0 = job.transport.clone();
        let work = match background.run(move || job.sound.play_from(job.start, &job.transport)) {
            Ok(work) => work,
            Err(error) => {
                state
                    .borrow_mut()
                    .completed(Err(io::Error::other(error.to_string())));
                break;
            }
        };
        let result = work.await;
        let next = state.borrow_mut().completed(result);
        match next {
            Some(next) => job = next,
            None => break,
        }
    }
}
