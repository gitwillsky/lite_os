import React, { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { list, open } from "lite:fs";
import type { FsEntry } from "lite:fs";

const AUDIO_EXTENSIONS = new Set([
  "wav", "wave", "aif", "aiff", "caf", "flac", "mp1", "mp2", "mp3",
  "ogg", "oga", "m4a", "mp4", "mka", "webm",
]);

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

function formatTime(value: number) {
  if (!Number.isFinite(value)) return "--:--";
  const seconds = Math.max(0, Math.floor(value));
  return `${Math.floor(seconds / 60)}:${String(seconds % 60).padStart(2, "0")}`;
}

export default function MusicPlayer() {
  const audio = useRef<LiteAudioElement>(null);
  const objectUrl = useRef<string | null>(null);
  const [path, setPath] = useState("/root/Music");
  const [entries, setEntries] = useState<FsEntry[]>([]);
  const [selected, setSelected] = useState(-1);
  const [duration, setDuration] = useState(Number.NaN);
  const [position, setPosition] = useState(0);
  const [loop, setLoop] = useState(false);
  const [volume, setVolume] = useState(0.8);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    const result = list(path);
    if (result.error) {
      setEntries([]);
      setError(`${path}: ${result.error}`);
      return;
    }
    setEntries((result.entries ?? []).slice().sort((left, right) => {
      if ((left.kind === "dir") !== (right.kind === "dir")) return left.kind === "dir" ? -1 : 1;
      return left.name.localeCompare(right.name);
    }));
    setSelected(-1);
    setError(result.truncated ? "The directory contains more entries than can be displayed." : null);
  }, [path]);

  useEffect(() => () => {
    audio.current?.pause();
    if (objectUrl.current) URL.revokeObjectURL(objectUrl.current);
  }, []);

  useEffect(() => {
    if (audio.current) audio.current.volume = volume;
  }, [volume]);

  const playlist = useMemo(() => entries.filter(isAudio), [entries]);

  const selectTrack = useCallback((index: number, play: boolean) => {
    const entry = playlist[index];
    if (!entry) return;
    try {
      audio.current?.pause();
      if (objectUrl.current) URL.revokeObjectURL(objectUrl.current);
      const file = open(joinPath(path, entry.name));
      const url = URL.createObjectURL(file);
      objectUrl.current = url;
      setSelected(index);
      setPosition(0);
      setDuration(Number.NaN);
      setError(null);
      if (audio.current) {
        audio.current.src = url;
        if (play) void audio.current.play().catch((reason: unknown) => {
          setError(reason instanceof Error ? reason.message : String(reason));
        });
      }
    } catch (reason) {
      setError(reason instanceof Error ? reason.message : String(reason));
    }
  }, [path, playlist]);

  const step = useCallback((delta: number, play = true) => {
    if (playlist.length === 0) return;
    const origin = selected < 0 ? (delta > 0 ? -1 : 0) : selected;
    selectTrack((origin + delta + playlist.length) % playlist.length, play);
  }, [playlist.length, selectTrack, selected]);

  const toggle = () => {
    const element = audio.current;
    if (!element) return;
    if (selected < 0) {
      selectTrack(0, true);
    } else if (element.paused) {
      void element.play().catch((reason: unknown) => setError(String(reason)));
    } else {
      element.pause();
    }
  };

  return (
    <div className="player">
      <div className="player__toolbar">
        <div className="player__button" onClick={() => setPath(parentPath(path))}>Up</div>
        <div className="player__address">{path}</div>
        <div className="player__button" onClick={() => setPath("/root/Music")}>My Music</div>
      </div>

      <div className="player__body">
        <div className="player__browser">
          {entries.map((entry) => {
            const trackIndex = playlist.indexOf(entry);
            const active = trackIndex === selected;
            return (
              <div
                key={entry.name}
                className={`player__row${active ? " player__row--selected" : ""}`}
                onDoubleClick={() => entry.kind === "dir"
                  ? setPath(joinPath(path, entry.name))
                  : trackIndex >= 0 && selectTrack(trackIndex, true)}
                onClick={() => trackIndex >= 0 && setSelected(trackIndex)}
              >
                <img
                  className="player__icon"
                  src={entry.kind === "dir" ? "assets/folder.png" : "assets/file-16.png"}
                />
                <span className="player__name">{entry.name}</span>
                <span className="player__kind">{entry.kind === "dir" ? "Folder" : isAudio(entry) ? "Audio" : "Unsupported"}</span>
              </div>
            );
          })}
        </div>

        <div className="player__now">
          <img className="player__art" src="assets/speaker.png"/>
          <span className="player__track">{playlist[selected]?.name ?? "Choose a track"}</span>
          <span className="player__meta">{formatTime(position)} / {formatTime(duration)}</span>
          <div className="player__transport">
            <div className="player__button" onClick={() => step(-1)}>Previous</div>
            <div className="player__button player__button--primary" onClick={toggle}>Play / Pause</div>
            <div className="player__button" onClick={() => step(1)}>Next</div>
          </div>
          <div className="player__transport">
            <div className="player__button" onClick={() => {
              if (audio.current) audio.current.currentTime = Math.max(0, position - 10);
            }}>-10 s</div>
            <div className="player__button" onClick={() => setLoop((value) => !value)}>
              {loop ? "Loop: on" : "Loop: off"}
            </div>
            <div className="player__button" onClick={() => setVolume((value) => Math.max(0, value - 0.1))}>Vol -</div>
            <span className="player__volume">{Math.round(volume * 100)}%</span>
            <div className="player__button" onClick={() => setVolume((value) => Math.min(1, value + 0.1))}>Vol +</div>
          </div>
          {error && <span className="player__error">{error}</span>}
        </div>
      </div>

      <audio
        ref={audio}
        controls
        preload="metadata"
        loop={loop}
        onLoadedMetadata={(event) => setDuration((event.currentTarget as unknown as LiteAudioElement).duration)}
        onTimeUpdate={(event) => setPosition((event.currentTarget as unknown as LiteAudioElement).currentTime)}
        onEnded={() => !loop && step(1)}
        onError={(event) => setError((event.currentTarget as unknown as LiteAudioElement).error?.message ?? "Unsupported or damaged audio file")}
      />
    </div>
  );
}
