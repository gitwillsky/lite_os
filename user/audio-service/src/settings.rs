use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

const MAGIC: &str = "LITEOS_AUDIO_V1";
const DEFAULT_PERCENT: u8 = 75;
const COALESCE: Duration = Duration::from_millis(500);

/// Authoritative persisted system volume.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct MasterState {
    pub(crate) percent: u8,
    pub(crate) muted: bool,
}

impl Default for MasterState {
    fn default() -> Self {
        Self {
            percent: DEFAULT_PERCENT,
            muted: false,
        }
    }
}

impl MasterState {
    pub(crate) fn gain(self) -> f32 {
        if self.muted || self.percent == 0 {
            0.0
        } else {
            (f32::from(self.percent) / 100.0).powi(3)
        }
    }
}

pub(crate) struct Settings {
    path: PathBuf,
    state: MasterState,
    // Owns the 500 ms coalescing deadline. Without it every slider movement
    // would synchronously fsync and stall the control plane.
    dirty_since: Option<Instant>,
}

impl Settings {
    pub(crate) fn load(path: PathBuf) -> io::Result<Self> {
        let state = match fs::read_to_string(&path) {
            Ok(contents) => match parse(&contents) {
                Some(state) => state,
                None => {
                    eprintln!(
                        "audio-service: preserving corrupt master settings at {}",
                        path.display()
                    );
                    MasterState::default()
                }
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => MasterState::default(),
            Err(error) => return Err(error),
        };
        Ok(Self {
            path,
            state,
            dirty_since: None,
        })
    }

    pub(crate) const fn state(&self) -> MasterState {
        self.state
    }

    pub(crate) fn set_percent(&mut self, percent: u8, now: Instant) -> bool {
        if percent > 100 || percent == self.state.percent {
            return false;
        }
        self.state.percent = percent;
        self.dirty_since = Some(now);
        true
    }

    pub(crate) fn set_muted(&mut self, muted: bool, now: Instant) -> bool {
        if muted == self.state.muted {
            return false;
        }
        self.state.muted = muted;
        self.dirty_since = Some(now);
        true
    }

    pub(crate) fn timeout(&self, now: Instant) -> Option<Duration> {
        self.dirty_since
            .map(|changed| (changed + COALESCE).saturating_duration_since(now))
    }

    pub(crate) fn flush_if_due(&mut self, now: Instant) -> io::Result<()> {
        if self
            .dirty_since
            .is_some_and(|changed| now.duration_since(changed) >= COALESCE)
        {
            self.flush()?;
        }
        Ok(())
    }

    pub(crate) fn flush(&mut self) -> io::Result<()> {
        if self.dirty_since.is_none() {
            return Ok(());
        }
        let parent = self.path.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "settings path has no parent")
        })?;
        fs::create_dir_all(parent)?;
        let temporary = temporary_path(&self.path);
        let result = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)?;
            write!(
                file,
                "{MAGIC}\npercent={}\nmuted={}\n",
                self.state.percent,
                u8::from(self.state.muted)
            )?;
            file.sync_all()?;
            fs::rename(&temporary, &self.path)?;
            File::open(parent)?.sync_all()
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        } else {
            self.dirty_since = None;
        }
        result
    }
}

fn parse(contents: &str) -> Option<MasterState> {
    let mut lines = contents.lines();
    if lines.next()? != MAGIC {
        return None;
    }
    let percent = lines.next()?.strip_prefix("percent=")?.parse::<u8>().ok()?;
    let muted = match lines.next()?.strip_prefix("muted=")? {
        "0" => false,
        "1" => true,
        _ => return None,
    };
    if percent > 100 || lines.next().is_some() {
        return None;
    }
    Some(MasterState { percent, muted })
}

fn temporary_path(path: &Path) -> PathBuf {
    let mut temporary = path.as_os_str().to_owned();
    temporary.push(format!(".tmp.{}", std::process::id()));
    PathBuf::from(temporary)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "liteos-audio-{name}-{}-{}",
            std::process::id(),
            Instant::now().elapsed().as_nanos()
        ))
    }

    #[test]
    fn default_curve_and_atomic_round_trip() {
        let path = unique_path("settings");
        let mut settings = Settings::load(path.clone()).expect("load default");
        assert_eq!(settings.state(), MasterState::default());
        let now = Instant::now();
        assert!(!settings.set_percent(75, now));
        assert!(!settings.set_percent(101, now));
        assert!(!settings.set_muted(false, now));
        assert_eq!(settings.state(), MasterState::default());
        assert_eq!(settings.timeout(now), None);
        assert!(settings.set_percent(50, now));
        assert!(settings.set_muted(true, now));
        assert!(settings.timeout(now).is_some_and(|delay| delay == COALESCE));
        settings.flush().expect("flush");
        let loaded = Settings::load(path.clone()).expect("reload");
        assert_eq!(
            loaded.state(),
            MasterState {
                percent: 50,
                muted: true
            }
        );
        fs::remove_file(path).expect("cleanup");
    }

    #[test]
    fn corrupt_file_is_preserved_and_uses_safe_default() {
        let path = unique_path("corrupt");
        fs::write(&path, "broken").expect("fixture");
        let settings = Settings::load(path.clone()).expect("load corrupt");
        assert_eq!(settings.state(), MasterState::default());
        assert_eq!(fs::read_to_string(&path).expect("preserved"), "broken");
        fs::remove_file(path).expect("cleanup");
    }

    #[test]
    fn continuous_changes_commit_only_after_five_hundred_ms() {
        let path = unique_path("coalesce");
        let mut settings = Settings::load(path.clone()).expect("load");
        let changed = Instant::now();
        assert!(settings.set_percent(25, changed));
        settings
            .flush_if_due(changed + Duration::from_millis(499))
            .expect("not due");
        assert!(!path.exists());
        settings.flush_if_due(changed + COALESCE).expect("due");
        assert_eq!(
            Settings::load(path.clone()).expect("reload").state(),
            MasterState {
                percent: 25,
                muted: false
            }
        );
        fs::remove_file(path).expect("cleanup");
    }
}
