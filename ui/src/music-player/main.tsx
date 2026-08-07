import React, { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { list, open } from "lite:fs";
import type { FsEntry } from "lite:fs";
import * as net from "lite:net";
import { RangeInput, TextInput } from "../design-system/controls";

const MUSIC_ROOT = "/root/Music";
const AUDIO_EXTENSIONS = new Set([
  "wav", "wave", "aif", "aiff", "caf", "flac", "mp1", "mp2", "mp3",
  "ogg", "oga", "m4a", "mp4", "mka", "webm",
]);
const KEY_ENTER = 28;
const KEY_ESC = 1;
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
// Mirrors NETEASE_LEVELS in user/apps/music-player/src/provider.rs.
const NETEASE_LEVELS = ["jymaster", "hires", "lossless", "exhigh", "standard"];
// QQ tier count; mirrors QUALITY_FILENAMES in user/apps/music-player/src/qq.rs.
const QQ_QUALITY_TIERS = 10;

interface SongUrlResolution {
  url: string;
  trial: boolean;
  // Platform/network rejection (stop trying lower tiers); null means this
  // quality tier is simply unavailable (try the next one).
  reason: string | null;
}

// Interprets one songUrl reply: transport error, HTTP status, and the
// provider's structured {url, kind, reason} body.
function parseSongUrlReply(reply: net.NetReply): SongUrlResolution {
  if (reply.error) return { url: "", trial: false, reason: reply.error };
  if (reply.status && reply.status >= 400) {
    return { url: "", trial: false, reason: `HTTP ${reply.status}` };
  }
  const body = reply.body ? JSON.parse(reply.body) : {};
  return {
    url: body.url ?? "",
    trial: body.kind === "trial",
    reason: body.reason ?? null,
  };
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

function localTrack(path: string, name: string): Track {
  const stem = name.replace(/\.[^.]+$/, "");
  const separator = stem.indexOf("-");
  const [title, artist] = separator <= 0 || separator === stem.length - 1
    ? [stem, "Local music"]
    : [stem.slice(0, separator).trim(), stem.slice(separator + 1).trim()];
  return { kind: "local", title, artist, src: path };
}

function remoteTrack(result: RemoteResult): Track {
  return {
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

/** Builds one no-repeat shuffle pass with the current track first. */
function shuffledOrder(length: number, current: number): number[] {
  const order = Array.from({ length }, (_, index) => index).filter((index) => index !== current);
  for (let index = order.length - 1; index > 0; index -= 1) {
    const swap = Math.floor(Math.random() * (index + 1));
    [order[index], order[swap]] = [order[swap], order[index]];
  }
  return current >= 0 && current < length ? [current, ...order] : order;
}

function PlayerButton({ label, icon, active, primary, disabled, onClick }: {
  label: string;
  icon?: string;
  active?: boolean;
  primary?: boolean;
  disabled?: boolean;
  onClick: () => void;
}) {
  const className = `player-button${active ? " player-button--active" : ""}${primary ? " player-button--primary" : ""}`;
  return (
    <button className={className} aria-pressed={active} disabled={disabled} onClick={onClick}>
      {icon && <img className="player-button__icon" src={icon} alt=""/>}
      <span className="control-label">{label}</span>
    </button>
  );
}

export default function MusicPlayer() {
  const audio = useRef<LiteAudioElement>(null);
  const objectUrl = useRef<string | null>(null);
  const activeStream = useRef<number | null>(null);
  // Monotonic request owners prevent an older search or track resolution from
  // publishing after a newer user action. Without them, a slow response can
  // replace newer results or begin playing the previously selected track.
  const searchGeneration = useRef(0);
  const playbackGeneration = useRef(0);
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
  // True while the current stream is a short trial clip, not the full track.
  const [trialClip, setTrialClip] = useState(false);

  // Local library browser state.
  const [browserPath, setBrowserPath] = useState(MUSIC_ROOT);
  const [browserEntries, setBrowserEntries] = useState<FsEntry[]>([]);
  const [browserError, setBrowserError] = useState<string | null>(null);
  const [browserNotice, setBrowserNotice] = useState<string | null>(null);

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
  const queueRef = useRef(queue);
  queueRef.current = queue;
  const shuffleRef = useRef(shuffle);
  shuffleRef.current = shuffle;
  // The shuffle deck and cursor preserve a no-repeat pass and make Previous
  // retrace actual history. Without them, random selection repeats tracks and
  // cannot honor Repeat Off at the end of one pass.
  const shuffleDeck = useRef<number[]>([]);
  const shuffleCursor = useRef(0);

  const resetShuffle = useCallback((length: number, current: number) => {
    const order = shuffledOrder(length, current);
    shuffleDeck.current = order;
    shuffleCursor.current = 0;
    return order;
  }, []);

  const closeStream = useCallback(() => {
    if (activeStream.current !== null) {
      net.streamClose(activeStream.current);
      activeStream.current = null;
    }
    setBuffering(null);
  }, []);

  // Points the audio element at a src and optionally plays.
  const playSrc = useCallback((src: string, play: boolean, generation: number) => {
    const element = audio.current;
    if (!element) return;
    element.pause();
    element.src = src;
    setPosition(0);
    setDuration(Number.NaN);
    setPlaybackError(null);
    if (play) {
      void element.play().catch((reason: unknown) => {
        if (playbackGeneration.current === generation) setPlaybackError(message(reason));
      });
    }
  }, []);

  // Resolves a remote track's playable URL (highest quality first), opens a
  // stream, and points the audio element at it.
  const resolveAndStream = useCallback(async (track: Track, generation: number) => {
    if (!track.source || !track.id) return;
    setResolving(true);
    setPlaybackError(null);
    try {
      let url = "";
      let trial = false;
      let reason: string | null = null;
      const tiers = track.source === "netease" ? NETEASE_LEVELS.length : QQ_QUALITY_TIERS;
      for (let tier = 0; tier < tiers; tier += 1) {
        const reply = await net.songUrl(track.source === "netease"
          ? { source: "netease", id: track.id, level: NETEASE_LEVELS[tier] }
          : { source: "qq", id: track.id, qualityIndex: tier });
        if (playbackGeneration.current !== generation) return;
        const resolution = parseSongUrlReply(reply);
        if (resolution.reason) {
          reason = resolution.reason;
          break;
        }
        if (resolution.url) {
          url = resolution.url;
          trial = resolution.trial;
          break;
        }
      }
      if (!url) {
        setResolving(false);
        setPlaybackError(reason ?? (track.vip
          ? "This track is VIP-only and could not be resolved. Try the other source."
          : "No playable URL was returned for this track."));
        return;
      }
      setTrialClip(trial);
      const ext = extFromUrl(url);
      const streamId = net.streamOpen(url, ext);
      if (playbackGeneration.current !== generation) {
        net.streamClose(streamId);
        return;
      }
      activeStream.current = streamId;
      net.watchStream(streamId, (event) => {
        if (playbackGeneration.current !== generation || activeStream.current !== streamId) return;
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
        playSrc(`stream:${streamId}`, true, generation);
      } else {
        // moov-at-tail container: wait for full download, then play.
        await new Promise<void>((resolve) => {
          const tick = () => {
            if (playbackGeneration.current !== generation) {
              resolve();
              return;
            }
            const stat = net.streamStat(streamId);
            if (stat.done || stat.error) resolve();
            else setTimeout(tick, 200);
          };
          tick();
        });
        if (playbackGeneration.current !== generation) return;
        playSrc(`stream:${streamId}`, true, generation);
      }
    } catch (reason) {
      if (playbackGeneration.current !== generation) return;
      setResolving(false);
      setPlaybackError(message(reason));
    }
  }, [playSrc]);

  // Activates queue[index]: routes local vs remote playback.
  const activate = useCallback((tracks: Track[], index: number, play: boolean) => {
    const track = tracks[index];
    if (!track) return;
    const queueChanged = queueRef.current !== tracks;
    queueRef.current = tracks;
    if (shuffleRef.current) {
      const position = queueChanged ? -1 : shuffleDeck.current.indexOf(index);
      if (position >= 0) shuffleCursor.current = position;
      else resetShuffle(tracks.length, index);
    }
    setQueue(tracks);
    setCurrentIndex(index);
    setView("now-playing");
    setTrialClip(false);
    const generation = playbackGeneration.current + 1;
    playbackGeneration.current = generation;
    if (audio.current) {
      audio.current.pause();
      // A remote URL is resolved asynchronously. Keeping the previous src here
      // makes Play after a resolution failure restart the old track.
      audio.current.src = "";
    }
    closeStream();
    if (objectUrl.current) {
      URL.revokeObjectURL(objectUrl.current);
      objectUrl.current = null;
    }
    setPosition(0);
    setDuration(Number.NaN);
    setPlaybackError(null);
    if (track.kind === "local") {
      try {
        setResolving(false);
        const file = open(track.src);
        const url = URL.createObjectURL(file);
        objectUrl.current = url;
        playSrc(url, play, generation);
      } catch (reason) {
        setPlaybackError(message(reason));
      }
    } else if (track.source && track.id) {
      void resolveAndStream(track, generation);
    }
  }, [closeStream, playSrc, resetShuffle, resolveAndStream]);

  // --- Local library browsing ---
  useEffect(() => {
    const result = list(browserPath);
    if (result.error) {
      setBrowserEntries([]);
      setBrowserError(`${browserPath}: ${result.error}`);
      setBrowserNotice(null);
      return;
    }
    const entries = (result.entries ?? [])
      .filter((entry) => !entry.name.startsWith(".") && (entry.kind === "dir" || isAudio(entry)))
      .slice().sort((left, right) => {
      if ((left.kind === "dir") !== (right.kind === "dir")) return left.kind === "dir" ? -1 : 1;
      return left.name.localeCompare(right.name);
    });
    setBrowserEntries(entries);
    setBrowserError(null);
    setBrowserNotice(result.truncated
      ? "The directory contains more entries than can be displayed."
      : null);
  }, [browserPath]);

  useEffect(() => () => {
    searchGeneration.current += 1;
    playbackGeneration.current += 1;
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
    const generation = searchGeneration.current + 1;
    searchGeneration.current = generation;
    setSearching(true);
    setSearchError(null);
    setResults([]);
    try {
      const reply = await net.search(source, trimmed, 25);
      if (searchGeneration.current !== generation) return;
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
      if (searchGeneration.current !== generation) return;
      setSearchError(message(reason));
    }
    if (searchGeneration.current === generation) setSearching(false);
  }, [query, source]);

  const selectSource = useCallback((next: Source) => {
    if (next === source) return;
    // Cancel publication from the old provider; otherwise its slower response
    // can repopulate results after the source indicator has already changed.
    searchGeneration.current += 1;
    setSource(next);
    setSearching(false);
    setResults([]);
    setSearchError(null);
  }, [source]);

  const clearSearch = useCallback(() => {
    // Invalidating the request owner prevents a late provider response from
    // repopulating results after the user has visibly dismissed the search.
    searchGeneration.current += 1;
    setSearching(false);
    setQuery("");
    setResults([]);
    setSearchError(null);
  }, []);

  const playRemoteResult = useCallback((result: RemoteResult) => {
    const tracks = results.map(remoteTrack);
    const index = results.findIndex((entry) => entry.source === result.source && entry.id === result.id);
    if (index >= 0) activate(tracks, index, true);
  }, [activate, results]);

  const togglePlayback = useCallback(() => {
    const element = audio.current;
    if (!element) return;
    if (currentIndex < 0 && queue.length > 0) {
      activate(queue, 0, true);
    } else if (element.paused && !element.src && currentIndex >= 0) {
      // Retrying a failed local open or remote URL resolution must rebuild the
      // current source; play() on an empty element cannot recover it.
      activate(queue, currentIndex, true);
    } else if (element.paused) {
      const generation = playbackGeneration.current;
      void element.play().catch((reason: unknown) => {
        if (playbackGeneration.current === generation) setPlaybackError(message(reason));
      });
    } else {
      element.pause();
    }
  }, [activate, currentIndex, queue]);

  const stepTrack = useCallback((delta: number) => {
    if (queue.length === 0) return;
    if (shuffle && queue.length > 1) {
      let position = shuffleCursor.current + (delta > 0 ? 1 : -1);
      if (position < 0 || position >= shuffleDeck.current.length) {
        const order = resetShuffle(queue.length, currentIndex);
        position = delta > 0 ? 1 : order.length - 1;
      }
      shuffleCursor.current = position;
      activate(queue, shuffleDeck.current[position], true);
      return;
    }
    const base = currentIndex < 0 ? (delta > 0 ? -1 : 0) : currentIndex;
    const index = (base + delta + queue.length) % queue.length;
    activate(queue, index, true);
  }, [activate, currentIndex, queue, resetShuffle, shuffle]);

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
      const position = shuffleCursor.current + 1;
      if (position < shuffleDeck.current.length) {
        shuffleCursor.current = position;
        activate(queue, shuffleDeck.current[position], true);
      } else if (repeat === "all") {
        const order = resetShuffle(queue.length, currentIndex);
        const next = order.length > 1 ? 1 : 0;
        shuffleCursor.current = next;
        activate(queue, order[next], true);
      }
      return;
    }
    if (currentIndex + 1 < queue.length) activate(queue, currentIndex + 1, true);
    else if (repeat === "all") activate(queue, 0, true);
  }, [activate, currentIndex, queue, repeat, resetShuffle, shuffle]);

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
  const toggleShuffle = () => {
    const next = !shuffleRef.current;
    shuffleRef.current = next;
    setShuffle(next);
    if (next) resetShuffle(queueRef.current.length, currentIndex);
    else {
      shuffleDeck.current = [];
      shuffleCursor.current = 0;
    }
  };

  const handleKey = (rawEvent: unknown) => {
    const event = rawEvent as LiteKeyEvent;
    if (event.value === 0) return;
    if (event.code === KEY_ESC && event.value === 1
      && view === "search" && (query || results.length > 0 || searchError || searching)) clearSearch();
    else if (event.code === KEY_SPACE && event.value === 1) togglePlayback();
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
              <PlayerButton label="NetEase" active={source === "netease"} onClick={() => selectSource("netease")}/>
              <PlayerButton label="QQ Music" active={source === "qq"} onClick={() => selectSource("qq")}/>
            </div>
            <TextInput
              className="player__search-input"
              value={query}
              placeholder="Search songs or artists..."
              onInput={setQuery}
              onKeyDown={(event) => {
                const key = event as unknown as LiteKeyEvent;
                if (key.value !== 1) return;
                if (key.code === KEY_ENTER) runSearch();
                else if (key.code === KEY_ESC && (query || results.length > 0 || searchError || searching)) clearSearch();
              }}
            />
            <PlayerButton label={searching ? "Searching..." : "Search"} icon="assets/search.png" primary disabled={searching || !query.trim()} onClick={runSearch}/>
          </div>
          <div className="player__results">
            {searching && <span className="player__empty">Searching {source === "netease" ? "NetEase" : "QQ Music"}...</span>}
            {!searching && !searchError && results.length === 0 && (
              <span className="player__empty">Search by song, artist, or album.</span>
            )}
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
          {trialClip && <span className="player__status">Trial clip</span>}
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
            <PlayerButton label="Prev" icon="assets/prev.png" disabled={queue.length === 0} onClick={() => stepTrack(-1)}/>
            <PlayerButton label={playing ? "Pause" : "Play"} icon={playing ? "assets/pause.png" : "assets/play.png"} primary disabled={resolving || (queue.length === 0 && !currentTrack)} onClick={togglePlayback}/>
            <PlayerButton label="Next" icon="assets/next.png" disabled={queue.length === 0} onClick={() => stepTrack(1)}/>
            <PlayerButton label={shuffle ? "Shuffle: On" : "Shuffle: Off"} active={shuffle} onClick={toggleShuffle}/>
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
                  {entry.kind === "dir" ? "Folder" : "Audio"}
                </span>
                <PlayerButton label={entry.kind === "dir" ? "Open" : "Play"} onClick={() => openBrowserEntry(entry)}/>
              </div>
            ))}
            {browserEntries.length === 0 && !browserError && (
              <span className="player__empty">No folders or supported audio files here.</span>
            )}
            {browserNotice && <span className="player__notice">{browserNotice}</span>}
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
          if (resolving) return;
          const element = event.currentTarget as unknown as LiteAudioElement;
          setPlaybackError(element.error?.message ?? "Unsupported or damaged audio file");
        }}
      />
    </div>
  );
}
