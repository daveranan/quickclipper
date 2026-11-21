export type FrameRequest = {
  clipEpoch: number
  requestId: number
  secondsAt: number
}

export type TimelineFrameSegment = {
  start: number
  end: number
  sourceStart: number
  sourceEnd: number
}

export type ViewerDrawSource = 'video' | 'still' | 'hold' | 'clear'

export type ViewerDrawState = {
  isPlaying: boolean
  isCScrubbing: boolean
  exactFrameReady: boolean
  exactFramePending: boolean
  suppressProxyCache: boolean
  videoDrawable: boolean
  videoCurrent: boolean
  stillDrawable: boolean
  hasPaintedContent: boolean
}

export function frameDuration(frameRate: number) {
  return 1 / Math.max(1, Number.isFinite(frameRate) ? frameRate : 1)
}

export function effectiveVideoFrameRate(
  clipFrameRate: number | null | undefined,
  fallbackFrameRate: number,
) {
  if (typeof clipFrameRate === 'number' && Number.isFinite(clipFrameRate) && clipFrameRate > 0) {
    return clipFrameRate
  }
  return Math.max(1, Number.isFinite(fallbackFrameRate) ? fallbackFrameRate : 30)
}

export function isFrameAtTime(actualSeconds: number, requestedSeconds: number, frameRate: number) {
  return Number.isFinite(actualSeconds)
    && Number.isFinite(requestedSeconds)
    && Math.abs(actualSeconds - requestedSeconds) <= Math.max(frameDuration(frameRate), 0.01)
}

export function isCurrentFrameRequest(
  request: FrameRequest,
  currentClipEpoch: number,
  currentRequestId: number,
  currentSeconds: number,
  frameRate: number,
) {
  return request.clipEpoch === currentClipEpoch
    && request.requestId === currentRequestId
    && isFrameAtTime(request.secondsAt, currentSeconds, frameRate)
}

export function cachedPreviewIndex(secondsAt: number, fps: number, frameCount: number) {
  if (!Number.isFinite(secondsAt) || secondsAt < 0 || !Number.isFinite(fps) || fps <= 0 || frameCount <= 0) {
    return null
  }
  const index = Math.round(secondsAt * fps)
  return index >= 0 && index < frameCount ? index : null
}

export function videoFrameSeekTime(sourceSeconds: number, frameRate: number, maxDuration?: number) {
  if (!Number.isFinite(sourceSeconds) || sourceSeconds <= 0) {
    return 0
  }
  // Nudge slightly inside the target frame boundary to avoid microsecond truncation landing one frame early.
  const nudge = Math.min(0.00005, frameDuration(frameRate) * 0.05)
  const target = sourceSeconds + nudge
  if (maxDuration !== undefined && Number.isFinite(maxDuration) && maxDuration > 0) {
    return Math.min(maxDuration, target)
  }
  return target
}

export function isVideoSettledForFrame(
  videoCurrentTime: number,
  expectedVideoTime: number,
  frameRate: number,
  strictExact: boolean,
) {
  if (!Number.isFinite(videoCurrentTime) || !Number.isFinite(expectedVideoTime)) {
    return false
  }
  if (strictExact) {
    const strictTolerance = Math.min(0.002, frameDuration(frameRate) * 0.25)
    return Math.abs(videoCurrentTime - expectedVideoTime) <= strictTolerance
  }
  return isFrameAtTime(videoCurrentTime, expectedVideoTime, frameRate)
}

export function resolvePreviewStillPath(options: {
  isPlaying: boolean
  exactFrame: string | null
  suppressProxyCache: boolean
  currentTime: number
  preview: { fps: number; frames: string[] } | null
}) {
  if (options.isPlaying && !options.exactFrame) {
    return null
  }
  if (options.exactFrame) {
    return options.exactFrame
  }
  if (options.suppressProxyCache) {
    return null
  }
  if (!options.preview || options.preview.frames.length === 0) {
    return null
  }
  const index = cachedPreviewIndex(options.currentTime, options.preview.fps, options.preview.frames.length)
  return index === null ? null : options.preview.frames[index]
}

export function selectViewerDrawSource(state: ViewerDrawState): ViewerDrawSource {
  if (state.isPlaying) {
    return state.videoDrawable ? 'video' : 'hold'
  }
  if (state.isCScrubbing) {
    if (state.suppressProxyCache) {
      return state.videoCurrent ? 'video' : 'hold'
    }
    return state.stillDrawable ? 'still' : 'hold'
  }
  if (state.exactFramePending) {
    if (state.hasPaintedContent) {
      return 'hold'
    }
    if (state.exactFrameReady && state.stillDrawable) {
      return 'still'
    }
    if (state.videoCurrent) {
      return 'video'
    }
    return state.videoDrawable ? 'video' : 'clear'
  }
  if (state.exactFrameReady && state.stillDrawable) {
    return 'still'
  }
  if (state.videoCurrent) {
    return 'video'
  }
  if (state.suppressProxyCache) {
    if (state.hasPaintedContent) {
      return 'hold'
    }
    return state.videoDrawable ? 'video' : 'clear'
  }
  if (state.stillDrawable) {
    return 'still'
  }
  if (state.videoDrawable && !state.hasPaintedContent) {
    return 'video'
  }
  if (!state.hasPaintedContent) {
    return 'clear'
  }
  return 'hold'
}

export function adjacentSourceFrame(
  currentSourceSeconds: number,
  direction: -1 | 1,
  frameRate: number,
  clips: TimelineFrameSegment[],
) {
  if (clips.length === 0) {
    return 0
  }
  const currentClip = clips.find((clip) => currentSourceSeconds >= clip.sourceStart && currentSourceSeconds <= clip.sourceEnd)
  const previousClip = [...clips].reverse().find((clip) => currentSourceSeconds >= clip.sourceEnd)
  const currentTimeline = currentClip
    ? currentClip.start + (currentSourceSeconds - currentClip.sourceStart)
    : previousClip?.end ?? clips[0].start
  const start = clips[0].start
  const end = clips[clips.length - 1].end
  const nextTimeline = Math.min(end, Math.max(start, currentTimeline + direction * frameDuration(frameRate)))
  const nextClip = clips.find((clip, index) => (
    nextTimeline >= clip.start
    && (index === clips.length - 1 ? nextTimeline <= clip.end : nextTimeline < clip.end)
  ))
  if (nextClip) {
    return nextClip.sourceStart + (nextTimeline - nextClip.start)
  }
  const priorClip = [...clips].reverse().find((clip) => nextTimeline >= clip.end)
  return priorClip?.sourceEnd ?? clips[0].sourceStart
}

export function sourceToEditedTime(sourceSeconds: number, clips: TimelineFrameSegment[]) {
  if (clips.length === 0 || !Number.isFinite(sourceSeconds)) {
    return 0
  }
  let elapsed = 0
  for (let index = 0; index < clips.length; index += 1) {
    const clip = clips[index]
    const clipLength = Math.max(0, clip.sourceEnd - clip.sourceStart)
    if (sourceSeconds <= clip.sourceStart) {
      return elapsed
    }
    if (sourceSeconds < clip.sourceEnd || (index === clips.length - 1 && sourceSeconds <= clip.sourceEnd)) {
      return elapsed + (sourceSeconds - clip.sourceStart)
    }
    elapsed += clipLength
  }
  return elapsed
}

export function normalizeSourceToSegments(sourceSeconds: number, clips: TimelineFrameSegment[]) {
  const safe = Number.isFinite(sourceSeconds) ? Math.max(0, sourceSeconds) : 0
  if (clips.length === 0) {
    return safe
  }
  for (let index = 0; index < clips.length; index += 1) {
    const clip = clips[index]
    if (safe < clip.sourceStart) {
      return clip.sourceStart
    }
    if (safe < clip.sourceEnd || (index === clips.length - 1 && safe <= clip.sourceEnd)) {
      return safe
    }
  }
  return clips[clips.length - 1].sourceEnd
}

export function timelineDurationForEdited(
  keptSeconds: number,
  timelineOffset: number,
  audioKeptSeconds = 0,
  audioTimelineOffset = 0,
) {
  return Math.max(1, Math.max(timelineOffset + keptSeconds, audioTimelineOffset + audioKeptSeconds) + 30)
}

export type VideoSeekPlan =
  | { action: 'queue'; target: number }
  | { action: 'already-at-target' }
  | { action: 'seek-now'; target: number }

export function planVideoSeek(options: {
  currentVideoTime: number
  targetVideoTime: number
  isSeeking: boolean
  coalesce: boolean
}): VideoSeekPlan {
  if (options.coalesce && options.isSeeking) {
    return { action: 'queue', target: options.targetVideoTime }
  }
  if (
    Number.isFinite(options.currentVideoTime)
    && Math.abs(options.currentVideoTime - options.targetVideoTime) <= 1e-6
  ) {
    return { action: 'already-at-target' }
  }
  return { action: 'seek-now', target: options.targetVideoTime }
}

export type SeekedFollowUpPlan = {
  paintNow: boolean
  nextSeekTarget: number | null
}

export function planSeekedFollowUp(options: {
  currentVideoTime: number
  pendingSeekTarget: number | null
  activelyScrubbing: boolean
}): SeekedFollowUpPlan {
  if (options.pendingSeekTarget === null) {
    return { paintNow: true, nextSeekTarget: null }
  }
  if (
    Number.isFinite(options.currentVideoTime)
    && Math.abs(options.currentVideoTime - options.pendingSeekTarget) <= 1e-6
  ) {
    return { paintNow: true, nextSeekTarget: null }
  }
  return {
    paintNow: options.activelyScrubbing,
    nextSeekTarget: options.pendingSeekTarget,
  }
}
