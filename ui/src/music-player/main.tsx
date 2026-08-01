import React, { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { list, open } from "lite:fs";
import type { FsEntry } from "lite:fs";
import * as net from "lite:net";
import { RangeInput } from "../design-system/controls";

const MUSIC_ROOT = "/root/Music";
const AUDIO_EXTENSIONS = new Set([
  "wav", "wave", "aif", "aiff", "caf", "flac", "mp1", "mp2", "mp3",
  "ogg", "oga", "m4a", "mp4", "mka", "webm",
]);
const KEY_ENTER = 28;
const KEY_SPACE = 57;
const KEY_UP = 103;
const KEY_LEFT = 105;
const KEY_RIGHT = 106;
const KEY_DOWN = 108;

type PlayerView = "search" | "now-playing" | "library";
type Source = "qq" | "netease";
type RepeatMode = "off" | "all" | "one";

// A track in the play queue: either a local file or a resolved remote stream.
interface Track {
  kind: "local" | "remote";
  title: string;
  artist: string;
  // Local: filesystem path. Remote: resolved lazily via source/id.
  src: string;
  source?: Source;
  id?: string;
  album?: string;
  cover?: string;
  durationMs?: number;
  vip?: boolean;
}

interface RemoteResult {
  source: Source;
  id: string;
  title: string;
  artist: string;
  album: string;
  durationMs: number;
  cover: string;
  vip: boolean;
}

// NetEase quality tiers to try, highest first (mp3 fallback always resolves).
const NETEASE_LEVELS = ["hires", "lossless", "exhigh", "standard"];

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

function localTrack(path: string, name: string): Track {
  const stem = name.replace(/\.[^.]+$/, "");
  const separator = stem.indexOf("-");
  const [title, artist] = separator <= 0 || separator === stem.length - 1
    ? [stem, "Local music"]
    : [stem.slice(0, separator).trim(), stem.slice(separator + 1).trim()];
  return { kind: "local", title, artist, src: path };
}

function localTracksAt(path: string, entries: FsEntry[]): Track[] {
  return entries.filter(isAudio).map((entry) =>
    localTrack(joinPath(path, entry.name), entry.name));
}

function formatTime(value: number) {
  if (!Number.isFinite(value) || value < 0) return "--:--";
  const seconds = Math.floor(value);
  return `${Math.floor(seconds / 60)}:${String(seconds % 60).padStart(2, "0")}`;
}

function message(reason: unknown) {
  return reason instanceof Error ? reason.message : String(reason);
}

// A container streams progressively only when its header sits at the front.
// MP4/M4A commonly carry `moov` at the tail, so download those fully first.
function containerStreams(ext: string) {
  return !["m4a", "mp4"].includes(ext.toLowerCase());
}

function extFromUrl(url: string): string {
  const clean = url.split("?")[0];
  const dot = clean.lastIndexOf(".");
  const ext = dot >= 0 ? clean.slice(dot + 1).toLowerCase() : "";
  return AUDIO_EXTENSIONS.has(ext) ? ext : "mp3";
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
  const activeStream = useRef<number | null>(null);
  const [view, setView] = useState<PlayerView>("search");

  // Online search state.
  const [source, setSource] = useState<Source>("netease");
  const [query, setQuery] = useState("");
  const [results, setResults] = useState<RemoteResult[]>([]);
  const [searching, setSearching] = useState(false);
  const [searchError, setSearchError] = useState<string | null>(null);

  // Streaming / resolution state.
  const [buffering, setBuffering] = useState<{ received: number; total: number } | null>(null);
  const [resolving, setResolving] = useState(false);

  // Local library browser state.
  const [browserPath, setBrowserPath] = useState(MUSIC_ROOT);
  const [browserEntries, setBrowserEntries] = useState<FsEntry[]>([]);
  const [browserError, setBrowserError] = useState<string | null>(null);

  // Playback state.
  const [queue, setQueue] = useState<Track[]>([]);
  const [currentIndex, setCurrentIndex] = useState(-1);
  const [duration, setDuration] = useState(Number.NaN);
  const [position, setPosition] = useState(0);
  const [playing, setPlaying] = useState(false);
  const [shuffle, setShuffle] = useState(false);
  const [repeat, setRepeat] = useState<RepeatMode>("off");
  const [volume, setVolume] = useState(0.8);
  const [muted, setMuted] = useState(false);
  const [playbackError, setPlaybackError] = useState<string | null>(null);

  const closeStream = useCallback(() => {
    if (activeStream.current !== null) {
      net.streamClose(activeStream.current);
      activeStream.current = null;
    }
    setBuffering(null);
  }, []);

  // Points the audio element at a src and optionally plays.
  const playSrc = useCallback((src: string, play: boolean) => {
    const element = audio.current;
    if (!element) return;
    element.pause();
    element.src = src;
    setPosition(0);
    setDuration(Number.NaN);
    setPlaybackError(null);
    if (play) {
      void element.play().catch((reason: unknown) => setPlaybackError(message(reason)));
    }
  }, []);

  // Resolves a remote track's playable URL (highest quality first), opens a
  // stream, and points the audio element at it.
  const resolveAndStream = useCallback(async (track: Track) => {
    if (!track.source || !track.id) return;
    closeStream();
    setResolving(true);
    setPlaybackError(null);
    try {
      let url = "";
      if (track.source === "netease") {
        for (const level of NETEASE_LEVELS) {
          const reply = await net.songUrl({ source: "netease", id: track.id, level });
          url = reply.body ? (JSON.parse(reply.body).url ?? "") : "";
          if (url) break;
        }
      } else {
        for (let qualityIndex = 0; qualityIndex < 3; qualityIndex += 1) {
          const reply = await net.songUrl({ source: "qq", id: track.id, qualityIndex });
          url = reply.body ? (JSON.parse(reply.body).url ?? "") : "";
          if (url) break;
        }
      }
      if (!url) {
        setResolving(false);
        setPlaybackError(track.vip
          ? "This track is VIP-only and could not be resolved. Try the other source."
          : "No playable URL was returned for this track.");
        return;
      }
      const ext = extFromUrl(url);
      const streamId = net.streamOpen(url, ext);
      activeStream.current = streamId;
      net.watchStream(streamId, (event) => {
        if (event.error) {
          setPlaybackError(event.error);
          setBuffering(null);
          return;
        }
        setBuffering({ received: event.received ?? 0, total: event.total ?? 0 });
        if (event.done) setBuffering(null);
      });
      setResolving(false);
      if (containerStreams(ext)) {
        playSrc(`stream:${streamId}`, true);
      } else {
        // moov-at-tail container: wait for full download, then play.
        await new Promise<void>((resolve) => {
          const tick = () => {
            const stat = net.streamStat(streamId);
            if (stat.done || stat.error) resolve();
            else setTimeout(tick, 200);
          };
          tick();
        });
        playSrc(`stream:${streamId}`, true);
      }
    } catch (reason) {
      setResolving(false);
      setPlaybackError(message(reason));
    }
  }, [closeStream, playSrc]);

  // Activates queue[index]: routes local vs remote playback.
  const activate = useCallback((tracks: Track[], index: number, play: boolean) => {
    const track = tracks[index];
    if (!track) return;
    setQueue(tracks);
    setCurrentIndex(index);
    setView("now-playing");
    if (track.kind === "local") {
      try {
        closeStream();
        if (objectUrl.current) URL.revokeObjectURL(objectUrl.current);
        const file = open(track.src);
        const url = URL.createObjectURL(file);
        objectUrl.current = url;
        playSrc(url, play);
      } catch (reason) {
        setPlaybackError(message(reason));
      }
    } else if (track.source && track.id) {
      void resolveAndStream(track);
    }
  }, [closeStream, playSrc, resolveAndStream]);

  // --- Local library browsing ---
  useEffect(() => {
    const result = list(browserPath);
    if (result.error) {
      setBrowserEntries([]);
      setBrowserError(`${browserPath}: ${result.error}`);
      return;
    }
    const entries = (result.entries ?? []).slice().sort((left, right) => {
      if ((left.kind === "dir") !== (right.kind === "dir")) return left.kind === "dir" ? -1 : 1;
      return left.name.localeCompare(right.name);
    });
    setBrowserEntries(entries);
    setBrowserError(result.truncated
      ? "The directory contains more entries than can be displayed."
      : null);
  }, [browserPath]);

  useEffect(() => () => {
    audio.current?.pause();
    if (objectUrl.current) URL.revokeObjectURL(objectUrl.current);
    closeStream();
  }, [closeStream]);

  useEffect(() => {
    if (audio.current) {
      audio.current.volume = volume;
      audio.current.muted = muted;
    }
  }, [muted, volume]);

  const currentTrack = queue[currentIndex] ?? null;
  const localTracks = useMemo(
    () => localTracksAt(browserPath, browserEntries),
    [browserEntries, browserPath],
  );

  const runSearch = useCallback(async () => {
    const trimmed = query.trim();
    if (!trimmed) return;
    setSearching(true);
    setSearchError(null);
    setResults([]);
    try {
      const reply = await net.search(source, trimmed, 25);
      if (reply.error) {
        setSearchError(
          source === "qq"
            ? "QQ Music is currently unavailable. Try NetEase."
            : reply.error,
        );
        setSearching(false);
        return;
      }
      // The host returns a normalized RemoteTrack[] JSON body.
      const parsed: RemoteResult[] = reply.body ? JSON.parse(reply.body) : [];
      setResults(parsed);
      if (parsed.length === 0) setSearchError("No results.");
    } catch (reason) {
      setSearchError(message(reason));
    }
    setSearching(false);
  }, [query, source]);

  const playRemoteResult = useCallback((result: RemoteResult) => {
    const track: Track = {
      kind: "remote",
      title: result.title,
      artist: result.artist,
      src: "",
      source: result.source,
      id: result.id,
      album: result.album,
      cover: result.cover,
      durationMs: result.durationMs,
      vip: result.vip,
    };
    const locals = queue.filter((entry) => entry.kind === "local");
    activate([...locals, track], locals.length, true);
  }, [activate, queue]);

  const togglePlayback = useCallback(() => {
    const element = audio.current;
    if (!element) return;
    if (currentIndex < 0 && queue.length > 0) {
      activate(queue, 0, true);
    } else if (element.paused) {
      void element.play().catch((reason: unknown) => setPlaybackError(message(reason)));
    } else {
      element.pause();
    }
  }, [activate, currentIndex, queue]);

  const stepTrack = useCallback((delta: number) => {
    if (queue.length === 0) return;
    const base = currentIndex < 0 ? (delta > 0 ? -1 : 0) : currentIndex;
    const index = (base + delta + queue.length) % queue.length;
    activate(queue, index, true);
  }, [activate, currentIndex, queue]);

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
    if (currentIndex + 1 < queue.length) activate(queue, currentIndex + 1, true);
    else if (repeat === "all") activate(queue, 0, true);
  }, [activate, currentIndex, queue, repeat]);

  const openBrowserEntry = useCallback((entry: FsEntry) => {
    if (entry.kind === "dir") {
      setBrowserPath(joinPath(browserPath, entry.name));
      return;
    }
    if (!isAudio(entry)) return;
    const target = joinPath(browserPath, entry.name);
    const index = localTracks.findIndex((track) => track.src === target);
    if (index >= 0) activate(localTracks, index, true);
  }, [activate, browserPath, localTracks]);

  const cycleRepeat = () =>
    setRepeat((mode) => mode === "off" ? "all" : mode === "all" ? "one" : "off");

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
  const repeatLabel = repeat === "off" ? "Repeat: Off" : repeat === "all" ? "Repeat: All" : "Repeat: One";
  const bufferPercent = buffering && buffering.total > 0
    ? Math.min(100, Math.round((buffering.received / buffering.total) * 100))
    : null;

  return (
    <div className="aurora-root player" tabIndex={0} onKeyDown={handleKey}>
      <div className="player__tabs">
        <PlayerButton label="Search" active={view === "search"} onClick={() => setView("search")}/>
        <PlayerButton label="Now Playing" active={view === "now-playing"} onClick={() => setView("now-playing")}/>
        <PlayerButton label="Library" active={view === "library"} onClick={() => setView("library")}/>
        <div className="player__tabs-spacer"/>
        <span className="player__output">Output: LiteOS Audio</span>
      </div>

      {view === "search" && (
        <div className="player__search">
          <div className="player__searchbar">
            <div className="player__sources">
              <PlayerButton label="NetEase" active={source === "netease"} onClick={() => setSource("netease")}/>
              <PlayerButton label="QQ Music" active={source === "qq"} onClick={() => setSource("qq")}/>
            </div>
            <input
              className="player__search-input"
              value={query}
              placeholder="Search songs or artists..."
              onInput={(event) => setQuery((event as unknown as { value: string }).value)}
              onKeyDown={(event) => {
                const key = event as unknown as LiteKeyEvent;
                if (key.code === KEY_ENTER && key.value !== 0) runSearch();
              }}
            />
            <PlayerButton label={searching ? "Searching..." : "Search"} primary disabled={searching} onClick={runSearch}/>
          </div>
          <div className="player__results">
            {searchError && <span className="player__empty">{searchError}</span>}
            {results.map((result) => (
              <div
                key={`${result.source}:${result.id}`}
                className="player__result-row"
                onDoubleClick={() => playRemoteResult(result)}
              >
                <img
                  className="player__result-badge"
                  src={result.source === "netease" ? "assets/badge-netease.png" : "assets/badge-qq.png"}
                />
                <div className="player__result-copy">
                  <span className="player__result-title">
                    {result.title}
                    {result.vip && <span className="player__vip">VIP</span>}
                  </span>
                  <span className="player__result-meta">{result.artist} - {result.album}</span>
                </div>
                <span className="player__result-duration">{formatTime(result.durationMs / 1000)}</span>
                <PlayerButton label="Play" onClick={() => playRemoteResult(result)}/>
              </div>
            ))}
          </div>
        </div>
      )}

      {view === "now-playing" && (
        <div className="player__stage">
          {currentTrack?.cover && !currentTrack.cover.startsWith("http")
            ? <img className="player__cover" src={currentTrack.cover}/>
            : <img className="player__cover" src="assets/cover-placeholder.png"/>}
          <span className="player__title">{currentTrack?.title ?? "Choose a track"}</span>
          <span className="player__artist">{currentTrack?.artist ?? "Search online or open your library"}</span>
          {resolving && <span className="player__status">Resolving...</span>}
          {bufferPercent !== null && <span className="player__status">Buffering {bufferPercent}%</span>}
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
            <PlayerButton label="Prev" disabled={queue.length === 0} onClick={() => stepTrack(-1)}/>
            <PlayerButton label={playing ? "Pause" : "Play"} primary disabled={queue.length === 0 && !currentTrack} onClick={togglePlayback}/>
            <PlayerButton label="Next" disabled={queue.length === 0} onClick={() => stepTrack(1)}/>
            <PlayerButton label={shuffle ? "Shuffle: On" : "Shuffle: Off"} active={shuffle} onClick={() => setShuffle((value) => !value)}/>
            <PlayerButton label={repeatLabel} active={repeat !== "off"} onClick={cycleRepeat}/>
          </div>
          <div className="player__volume">
            <PlayerButton label={muted ? "Unmute" : "Mute"} active={muted} onClick={() => setMuted((value) => !value)}/>
            <RangeInput className="player__volume-range" min={0} max={100} step={1} value={volume * 100} onInput={(value) => changeVolume(value / 100)}/>
            <span>{Math.round(volume * 100)}%</span>
          </div>
          {playbackError && <span className="player__error">{playbackError}</span>}
        </div>
      )}

      {view === "library" && (
        <div className="player__library">
          <div className="player__librarybar">
            <PlayerButton label="Up" disabled={browserPath === "/"} onClick={() => setBrowserPath(parentPath(browserPath))}/>
            <div className="player__address">{browserPath}</div>
            <PlayerButton label="My Music" onClick={() => setBrowserPath(MUSIC_ROOT)}/>
          </div>
          <div className="player__browser">
            {browserEntries.map((entry) => (
              <div
                key={entry.name}
                className="player__browser-row"
                onDoubleClick={() => openBrowserEntry(entry)}
              >
                <img className="player__browser-icon" src={entry.kind === "dir" ? "assets/folder.png" : "assets/file-16.png"}/>
                <span className="player__browser-name">{entry.name}</span>
                <span className="player__browser-type">
                  {entry.kind === "dir" ? "Folder" : isAudio(entry) ? "Audio" : "Unsupported"}
                </span>
              </div>
            ))}
            {browserEntries.length === 0 && !browserError && <span className="player__empty">This folder is empty.</span>}
            {browserError && <span className="player__error">{browserError}</span>}
          </div>
        </div>
      )}

      <audio
        ref={audio}
        style={{ display: "none" }}
        preload="metadata"
        loop={repeat === "one"}
        onLoadedMetadata={(event) => setDuration((event.currentTarget as unknown as LiteAudioElement).duration)}
        onPlaying={() => setPlaying(true)}
        onPause={() => setPlaying(false)}
        onTimeUpdate={(event) => setPosition((event.currentTarget as unknown as LiteAudioElement).currentTime)}
        onEnded={handleEnded}
        onError={(event) => {
          const element = event.currentTarget as unknown as LiteAudioElement;
          setPlaybackError(element.error?.message ?? "Unsupported or damaged audio file");
        }}
      />
    </div>
  );
}
