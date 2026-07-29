import React, { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { list, open } from "lite:fs";
import type { FsEntry } from "lite:fs";
import { RangeInput } from "../design-system/controls";

const MUSIC_ROOT = "/root/Music";
const AUDIO_EXTENSIONS = new Set([
  "wav", "wave", "aif", "aiff", "caf", "flac", "mp1", "mp2", "mp3",
  "ogg", "oga", "m4a", "mp4", "mka", "webm",
]);
const KEY_SPACE = 57;
const KEY_UP = 103;
const KEY_LEFT = 105;
const KEY_RIGHT = 106;
const KEY_DOWN = 108;

type PlayerView = "now-playing" | "library";
type RepeatMode = "off" | "all" | "one";

interface Track {
  name: string;
  path: string;
}

function joinPath(directory: string, name: string) {
  return directory === "/" ? `/${name}` : `${directory}/${name}`;
}

function parentPath(path: string) {
  const normalized = path.replace(/\/+$/, "");
  const separator = normalized.lastIndexOf("/");
  return separator <= 0 ? "/" : normalized.slice(0, separator);
}

function isAudio(entry: FsEntry) {
  const extension = entry.name.slice(entry.name.lastIndexOf(".") + 1).toLowerCase();
  return entry.kind === "file" && AUDIO_EXTENSIONS.has(extension);
}

function tracksAt(path: string, entries: FsEntry[]): Track[] {
  return entries.filter(isAudio).map((entry) => ({
    name: entry.name,
    path: joinPath(path, entry.name),
  }));
}

function trackLabels(name: string) {
  const stem = name.replace(/\.[^.]+$/, "");
  const separator = stem.indexOf("-");
  if (separator <= 0 || separator === stem.length - 1) {
    return { title: stem, artist: "Local music" };
  }
  return {
    title: stem.slice(0, separator),
    artist: stem.slice(separator + 1),
  };
}

function formatTime(value: number) {
  if (!Number.isFinite(value) || value < 0) return "--:--";
  const seconds = Math.floor(value);
  return `${Math.floor(seconds / 60)}:${String(seconds % 60).padStart(2, "0")}`;
}

function message(reason: unknown) {
  return reason instanceof Error ? reason.message : String(reason);
}

function PlayerButton({ label, active, primary, disabled, onClick }: {
  label: string;
  active?: boolean;
  primary?: boolean;
  disabled?: boolean;
  onClick: () => void;
}) {
  const className = `player-button${active ? " player-button--active" : ""}${primary ? " player-button--primary" : ""}`;
  return (
    <button className={className} disabled={disabled} onClick={onClick}>
      <span>{label}</span>
    </button>
  );
}

export default function MusicPlayer() {
  const audio = useRef<LiteAudioElement>(null);
  const objectUrl = useRef<string | null>(null);
  const initialized = useRef(false);
  const [view, setView] = useState<PlayerView>("now-playing");
  const [browserPath, setBrowserPath] = useState(MUSIC_ROOT);
  const [browserEntries, setBrowserEntries] = useState<FsEntry[]>([]);
  const [browserSelection, setBrowserSelection] = useState<string | null>(null);
  const [browserError, setBrowserError] = useState<string | null>(null);
  const [queue, setQueue] = useState<Track[]>([]);
  const [currentIndex, setCurrentIndex] = useState(-1);
  const [duration, setDuration] = useState(Number.NaN);
  const [position, setPosition] = useState(0);
  const [playing, setPlaying] = useState(false);
  const [seeking, setSeeking] = useState(false);
  const [shuffle, setShuffle] = useState(false);
  const [repeat, setRepeat] = useState<RepeatMode>("off");
  const [volume, setVolume] = useState(0.8);
  const [muted, setMuted] = useState(false);
  const [settingsOpen, setSettingsOpen] = useState(false);
  const [playbackError, setPlaybackError] = useState<string | null>(null);

  const activateTrack = useCallback((tracks: Track[], index: number, play: boolean) => {
    const track = tracks[index];
    if (!track) return;
    try {
      audio.current?.pause();
      if (objectUrl.current) URL.revokeObjectURL(objectUrl.current);
      const file = open(track.path);
      const url = URL.createObjectURL(file);
      objectUrl.current = url;
      setQueue(tracks);
      setCurrentIndex(index);
      setPosition(0);
      setDuration(Number.NaN);
      setPlaying(false);
      setSeeking(false);
      setPlaybackError(null);
      setView("now-playing");
      if (audio.current) {
        audio.current.src = url;
        if (play) {
          void audio.current.play().catch((reason: unknown) => setPlaybackError(message(reason)));
        }
      }
    } catch (reason) {
      setPlaybackError(message(reason));
    }
  }, []);

  useEffect(() => {
    const result = list(browserPath);
    if (result.error) {
      setBrowserEntries([]);
      setBrowserSelection(null);
      setBrowserError(`${browserPath}: ${result.error}`);
      return;
    }
    const entries = (result.entries ?? []).slice().sort((left, right) => {
      if ((left.kind === "dir") !== (right.kind === "dir")) return left.kind === "dir" ? -1 : 1;
      return left.name.localeCompare(right.name);
    });
    setBrowserEntries(entries);
    setBrowserSelection(null);
    setBrowserError(result.truncated
      ? "The directory contains more entries than can be displayed."
      : null);
    if (!initialized.current) {
      initialized.current = true;
      const tracks = tracksAt(browserPath, entries);
      if (tracks.length > 0) activateTrack(tracks, 0, false);
    }
  }, [activateTrack, browserPath]);

  useEffect(() => () => {
    audio.current?.pause();
    if (objectUrl.current) URL.revokeObjectURL(objectUrl.current);
  }, []);

  useEffect(() => {
    if (audio.current) {
      audio.current.volume = volume;
      audio.current.muted = muted;
    }
  }, [muted, volume]);

  const browserTracks = useMemo(
    () => tracksAt(browserPath, browserEntries),
    [browserEntries, browserPath],
  );
  const currentTrack = queue[currentIndex] ?? null;
  const currentLabels = currentTrack
    ? trackLabels(currentTrack.name)
    : { title: "Choose a track", artist: "Open your music library to begin" };

  const randomIndex = useCallback(() => {
    if (queue.length < 2) return Math.max(0, currentIndex);
    const candidate = Math.floor(Math.random() * (queue.length - 1));
    return candidate >= currentIndex ? candidate + 1 : candidate;
  }, [currentIndex, queue.length]);

  const stepTrack = useCallback((delta: number) => {
    if (queue.length === 0) return;
    const index = shuffle
      ? randomIndex()
      : ((currentIndex < 0 ? (delta > 0 ? -1 : 0) : currentIndex) + delta + queue.length)
        % queue.length;
    activateTrack(queue, index, true);
  }, [activateTrack, currentIndex, queue, randomIndex, shuffle]);

  const togglePlayback = useCallback(() => {
    const element = audio.current;
    if (!element) return;
    if (currentIndex < 0) {
      if (queue.length > 0) activateTrack(queue, 0, true);
    } else if (element.paused) {
      void element.play().catch((reason: unknown) => setPlaybackError(message(reason)));
    } else {
      element.pause();
    }
  }, [activateTrack, currentIndex, queue]);

  const seekTo = useCallback((seconds: number) => {
    const element = audio.current;
    if (!element || !Number.isFinite(duration)) return;
    const target = Math.max(0, Math.min(duration, seconds));
    setPosition(target);
    element.currentTime = target;
  }, [duration]);

  const changeVolume = useCallback((next: number) => {
    setVolume(Math.max(0, Math.min(1, next)));
  }, []);

  const handleEnded = useCallback(() => {
    setPlaying(false);
    if (repeat === "one" || queue.length === 0) return;
    if (shuffle) {
      activateTrack(queue, randomIndex(), true);
    } else if (currentIndex + 1 < queue.length) {
      activateTrack(queue, currentIndex + 1, true);
    } else if (repeat === "all") {
      activateTrack(queue, 0, true);
    }
  }, [activateTrack, currentIndex, queue, randomIndex, repeat, shuffle]);

  const openBrowserEntry = useCallback((entry: FsEntry) => {
    if (entry.kind === "dir") {
      setBrowserPath(joinPath(browserPath, entry.name));
      return;
    }
    if (!isAudio(entry)) return;
    const index = browserTracks.findIndex((track) => track.name === entry.name);
    if (index >= 0) activateTrack(browserTracks, index, true);
  }, [activateTrack, browserPath, browserTracks]);

  const playBrowserSelection = () => {
    const entry = browserEntries.find((candidate) => candidate.name === browserSelection);
    if (entry) openBrowserEntry(entry);
  };

  const cycleRepeat = () => {
    setRepeat((mode) => mode === "off" ? "all" : mode === "all" ? "one" : "off");
  };

  const handleKey = (rawEvent: unknown) => {
    const event = rawEvent as LiteKeyEvent;
    if (event.value === 0) return;
    if (event.code === KEY_SPACE) togglePlayback();
    else if (event.code === KEY_LEFT) seekTo(position - 5);
    else if (event.code === KEY_RIGHT) seekTo(position + 5);
    else if (event.code === KEY_UP) changeVolume(volume + 0.05);
    else if (event.code === KEY_DOWN) changeVolume(volume - 0.05);
  };

  const seekMaximum = Number.isFinite(duration) && duration > 0 ? duration : 1;
  const seekValue = Number.isFinite(duration) ? Math.min(position, duration) : 0;
  const repeatLabel = repeat === "off" ? "Repeat: Off" : repeat === "all"
    ? "Repeat: All"
    : "Repeat: One";

  return (
    <div className="aurora-root player" tabIndex={0} onKeyDown={handleKey}>
      {view === "now-playing" ? (
        <>
          <div className="player__commandbar">
            <PlayerButton label="Back to Library" onClick={() => {
              setSettingsOpen(false);
              setView("library");
            }}/>
            <div className="player__commandbar-spacer"/>
            <PlayerButton
              label={shuffle ? "Shuffle: On" : "Shuffle: Off"}
              active={shuffle}
              onClick={() => setShuffle((value) => !value)}
            />
            <PlayerButton label={repeatLabel} active={repeat !== "off"} onClick={cycleRepeat}/>
            <PlayerButton
              label="Audio Settings"
              active={settingsOpen}
              onClick={() => setSettingsOpen((value) => !value)}
            />
          </div>

          {settingsOpen && (
            <div className="player__settings">
              <span className="player__settings-title">Application audio</span>
              <div className="player__settings-row">
                <span>Volume</span>
                <span>{Math.round(volume * 100)}%</span>
              </div>
              <RangeInput
                className="player__settings-range"
                min={0}
                max={100}
                step={1}
                value={volume * 100}
                onInput={(value) => changeVolume(value / 100)}
              />
              <PlayerButton
                label={muted ? "Unmute" : "Mute"}
                active={muted}
                onClick={() => setMuted((value) => !value)}
              />
            </div>
          )}

          <div className="player__workspace">
            <div className="player__stage">
              <span className="player__section-title">Now Playing</span>
              <div className="player__cover-frame">
                <img className="player__cover" src="assets/solar-system-cover.png"/>
              </div>
              <span className="player__title">{currentLabels.title}</span>
              <span className="player__artist">{currentLabels.artist}</span>
              <div className="player__seek-row">
                <span className="player__time">{formatTime(position)}</span>
                <RangeInput
                  className="player__seek"
                  min={0}
                  max={seekMaximum}
                  step={0.1}
                  value={seekValue}
                  disabled={!currentTrack || !Number.isFinite(duration)}
                  onInput={seekTo}
                />
                <span className="player__time player__time--end">{formatTime(duration)}</span>
              </div>
              <div className="player__transport">
                <PlayerButton label="Previous" disabled={queue.length === 0} onClick={() => stepTrack(-1)}/>
                <PlayerButton
                  label={playing ? "Pause" : "Play"}
                  primary
                  disabled={queue.length === 0}
                  onClick={togglePlayback}
                />
                <PlayerButton label="Next" disabled={queue.length === 0} onClick={() => stepTrack(1)}/>
              </div>
              <span className="player__playback-state">
                {seeking ? "Seeking..." : playing ? "Playing" : currentTrack ? "Paused" : "No track loaded"}
              </span>
              {playbackError && <span className="player__error">{playbackError}</span>}
            </div>

            <div className="player__queue">
              <div className="player__queue-header">
                <span>Up Next</span>
                <PlayerButton label="Open Folder" onClick={() => setView("library")}/>
              </div>
              <div className="player__queue-list">
                {queue.length === 0 && (
                  <span className="player__empty">No audio files in this folder.</span>
                )}
                {queue.map((track, index) => {
                  const labels = trackLabels(track.name);
                  const active = index === currentIndex;
                  return (
                    <div
                      key={track.path}
                      className={`player__queue-row${active ? " player__queue-row--active" : ""}`}
                      onClick={() => activateTrack(queue, index, true)}
                    >
                      <span className="player__queue-index">{active && playing ? ">" : String(index + 1)}</span>
                      <div className="player__queue-copy">
                        <span className="player__queue-title">{labels.title}</span>
                        <span className="player__queue-artist">{labels.artist}</span>
                      </div>
                      <span className="player__queue-duration">
                        {active ? formatTime(duration) : "--:--"}
                      </span>
                    </div>
                  );
                })}
              </div>
            </div>
          </div>

          <div className="player__statusbar">
            <div className="player__volume">
              <PlayerButton
                label={muted ? "Unmute" : "Mute"}
                active={muted}
                onClick={() => setMuted((value) => !value)}
              />
              <RangeInput
                className="player__volume-range"
                min={0}
                max={100}
                step={1}
                value={volume * 100}
                onInput={(value) => changeVolume(value / 100)}
              />
              <span>{Math.round(volume * 100)}%</span>
            </div>
            <span className="player__output">Output: LiteOS Audio</span>
          </div>
        </>
      ) : (
        <>
          <div className="player__librarybar">
            <PlayerButton label="Now Playing" onClick={() => setView("now-playing")}/>
            <PlayerButton
              label="Up"
              disabled={browserPath === "/"}
              onClick={() => setBrowserPath(parentPath(browserPath))}
            />
            <div className="player__address">{browserPath}</div>
            <PlayerButton label="My Music" onClick={() => setBrowserPath(MUSIC_ROOT)}/>
            <PlayerButton
              label="Play Selection"
              primary
              disabled={!browserSelection}
              onClick={playBrowserSelection}
            />
          </div>
          <div className="player__library">
            <div className="player__library-head">
              <span className="player__library-name">Name</span>
              <span className="player__library-type">Type</span>
            </div>
            <div className="player__browser">
              {browserEntries.map((entry) => {
                const selected = entry.name === browserSelection;
                return (
                  <div
                    key={entry.name}
                    className={`player__browser-row${selected ? " player__browser-row--selected" : ""}`}
                    onClick={() => setBrowserSelection(entry.name)}
                    onDoubleClick={() => openBrowserEntry(entry)}
                  >
                    <img
                      className="player__browser-icon"
                      src={entry.kind === "dir" ? "assets/folder.png" : "assets/file-16.png"}
                    />
                    <span className="player__browser-name">{entry.name}</span>
                    <span className="player__browser-type">
                      {entry.kind === "dir" ? "Folder" : isAudio(entry) ? "Audio" : "Unsupported"}
                    </span>
                  </div>
                );
              })}
              {browserEntries.length === 0 && !browserError && (
                <span className="player__empty">This folder is empty.</span>
              )}
            </div>
            {browserError && <span className="player__library-error">{browserError}</span>}
          </div>
          <div className="player__library-status">
            <span>{browserEntries.length} {browserEntries.length === 1 ? "item" : "items"}</span>
            <span>
              {browserTracks.length} audio {browserTracks.length === 1 ? "track" : "tracks"}
            </span>
          </div>
        </>
      )}

      <audio
        ref={audio}
        style={{ display: "none" }}
        preload="metadata"
        loop={repeat === "one"}
        onLoadedMetadata={(event) => {
          const element = event.currentTarget as unknown as LiteAudioElement;
          setDuration(element.duration);
        }}
        onPlaying={() => setPlaying(true)}
        onPause={() => setPlaying(false)}
        onSeeking={() => setSeeking(true)}
        onSeeked={() => setSeeking(false)}
        onTimeUpdate={(event) => {
          const element = event.currentTarget as unknown as LiteAudioElement;
          setPosition(element.currentTime);
        }}
        onEnded={handleEnded}
        onError={(event) => {
          const element = event.currentTarget as unknown as LiteAudioElement;
          setPlaybackError(element.error?.message ?? "Unsupported or damaged audio file");
        }}
      />
    </div>
  );
}
