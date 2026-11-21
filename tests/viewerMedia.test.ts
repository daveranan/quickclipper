import assert from 'node:assert/strict'
import test from 'node:test'

import {
  adjacentSourceFrame,
  cachedPreviewIndex,
  effectiveVideoFrameRate,
  frameDuration,
  isCurrentFrameRequest,
  isFrameAtTime,
  isVideoSettledForFrame,
  normalizeSourceToSegments,
  planSeekedFollowUp,
  planVideoSeek,
  resolvePreviewStillPath,
  selectViewerDrawSource,
  sourceToEditedTime,
  timelineDurationForEdited,
  videoFrameSeekTime,
} from '../src/viewerMedia.ts'

test('scrub cache selects the nearest available proxy frame', () => {
  assert.equal(cachedPreviewIndex(1.04, 12, 120), 12)
  assert.equal(cachedPreviewIndex(1.05, 12, 120), 13)
  assert.equal(cachedPreviewIndex(10, 12, 120), null)
  assert.equal(cachedPreviewIndex(-0.01, 12, 120), null)
})

test('settled video accepts one effective frame duration and rejects older frames', () => {
  assert.equal(frameDuration(30), 1 / 30)
  assert.equal(isFrameAtTime(2 + 1 / 30, 2, 30), true)
  assert.equal(isFrameAtTime(2 + 1 / 20, 2, 30), false)
})

test('exact frame completion must match clip, request, and current time', () => {
  const request = { clipEpoch: 4, requestId: 9, secondsAt: 12 }
  assert.equal(isCurrentFrameRequest(request, 4, 9, 12, 30), true)
  assert.equal(isCurrentFrameRequest(request, 5, 9, 12, 30), false)
  assert.equal(isCurrentFrameRequest(request, 4, 10, 12, 30), false)
  assert.equal(isCurrentFrameRequest(request, 4, 9, 12.1, 30), false)
})

test('frame stepping advances one timeline frame across cuts and clamps at bounds', () => {
  const clips = [
    { start: 0, end: 1, sourceStart: 0, sourceEnd: 1 },
    { start: 1, end: 2, sourceStart: 2, sourceEnd: 3 },
  ]
  assert.ok(Math.abs(adjacentSourceFrame(0.99, 1, 30, clips) - (2 + 0.99 + 1 / 30 - 1)) < 1e-9)
  assert.ok(Math.abs(adjacentSourceFrame(2.01, -1, 30, clips) - (1.01 - 1 / 30)) < 1e-9)
  assert.equal(adjacentSourceFrame(0, -1, 30, clips), 0)
  assert.equal(adjacentSourceFrame(3, 1, 30, clips), 3)
  // Landing exactly on the cut seam should enter the first frame after the cut, not the deleted boundary.
  assert.equal(adjacentSourceFrame(1 - 1 / 30, 1, 30, clips), 2)
  assert.ok(Math.abs(adjacentSourceFrame(2, -1, 30, clips) - (1 - 1 / 30)) < 1e-9)
})

test('effectiveVideoFrameRate prefers valid clip FPS and falls back cleanly', () => {
  assert.equal(effectiveVideoFrameRate(59.94, 30), 59.94)
  assert.equal(effectiveVideoFrameRate(null, 24), 24)
  assert.equal(effectiveVideoFrameRate(undefined, 60), 60)
  assert.equal(effectiveVideoFrameRate(0, 30), 30)
  assert.equal(effectiveVideoFrameRate(Number.NaN, 30), 30)
  assert.equal(effectiveVideoFrameRate(-1, Number.NaN), 30)
})

test('videoFrameSeekTime nudges inside the frame without crossing into the next frame', () => {
  assert.equal(videoFrameSeekTime(0, 30, 10), 0)
  assert.equal(videoFrameSeekTime(-0.5, 30, 10), 0)
  const f1 = videoFrameSeekTime(1 / 30, 30, 10)
  const f2 = videoFrameSeekTime(2 / 30, 30, 10)
  assert.ok(f1 > 1 / 30 && f1 < 2 / 30)
  assert.ok(f2 > 2 / 30 && f2 < 3 / 30)
  const f120 = videoFrameSeekTime(1 / 120, 120, 10)
  assert.ok(f120 > 1 / 120 && f120 < 2 / 120)
  assert.equal(videoFrameSeekTime(10, 30, 9.95), 9.95)
})

test('isVideoSettledForFrame rejects adjacent frames in strict exact-step mode', () => {
  const target = videoFrameSeekTime(2 / 30, 30, 10)
  assert.equal(isVideoSettledForFrame(1 / 30, target, 30, true), false)
  assert.equal(isVideoSettledForFrame(3 / 30, target, 30, true), false)
  assert.equal(isVideoSettledForFrame(target - 0.02, target, 30, true), false)
  assert.equal(isVideoSettledForFrame(target - 0.0005, target, 30, true), true)
  assert.equal(isVideoSettledForFrame(target - 0.02, target, 30, false), true)
})

test('resolvePreviewStillPath suppresses the 12fps proxy cache during stepping and exact loading', () => {
  const preview = { fps: 12, frames: ['f0.jpg', 'f1.jpg', 'f2.jpg'] }
  assert.equal(
    resolvePreviewStillPath({ isPlaying: false, exactFrame: null, suppressProxyCache: true, currentTime: 0.1, preview }),
    null,
  )
  assert.equal(
    resolvePreviewStillPath({ isPlaying: false, exactFrame: 'exact.jpg', suppressProxyCache: true, currentTime: 0.1, preview }),
    'exact.jpg',
  )
  assert.equal(
    resolvePreviewStillPath({ isPlaying: false, exactFrame: null, suppressProxyCache: false, currentTime: 0.083, preview }),
    'f1.jpg',
  )
  assert.equal(
    resolvePreviewStillPath({ isPlaying: true, exactFrame: null, suppressProxyCache: false, currentTime: 0.083, preview }),
    null,
  )
})

test('selectViewerDrawSource holds previous frame until true frame settles without flashing proxy', () => {
  const base = {
    isPlaying: false,
    isCScrubbing: false,
    exactFrameReady: false,
    exactFramePending: false,
    suppressProxyCache: false,
    videoDrawable: true,
    videoCurrent: false,
    stillDrawable: false,
    hasPaintedContent: true,
  }
  assert.equal(selectViewerDrawSource({ ...base, suppressProxyCache: true, stillDrawable: true }), 'hold')
  assert.equal(selectViewerDrawSource({ ...base, exactFramePending: true, exactFrameReady: true, stillDrawable: true }), 'hold')
  assert.equal(selectViewerDrawSource({ ...base, suppressProxyCache: true, videoCurrent: true }), 'video')
  assert.equal(selectViewerDrawSource({ ...base, suppressProxyCache: true, exactFrameReady: true, stillDrawable: true }), 'still')
  assert.equal(selectViewerDrawSource({ ...base, suppressProxyCache: true, hasPaintedContent: false, videoDrawable: false }), 'clear')
  assert.equal(selectViewerDrawSource({ ...base, isPlaying: true }), 'video')
  assert.equal(selectViewerDrawSource({ ...base, isCScrubbing: true, stillDrawable: true }), 'still')
  assert.equal(selectViewerDrawSource({ ...base, isCScrubbing: true, suppressProxyCache: true, stillDrawable: true }), 'hold')
  assert.equal(selectViewerDrawSource({ ...base, isCScrubbing: true, suppressProxyCache: true, videoCurrent: true, stillDrawable: true }), 'video')
})

test('planVideoSeek queues continuous scrub seeks without aborting in-flight decodes', () => {
  assert.deepEqual(
    planVideoSeek({ currentVideoTime: 1, targetVideoTime: 2.5, isSeeking: true, coalesce: true }),
    { action: 'queue', target: 2.5 },
  )
  assert.deepEqual(
    planVideoSeek({ currentVideoTime: 1, targetVideoTime: 2.5, isSeeking: false, coalesce: true }),
    { action: 'seek-now', target: 2.5 },
  )
  assert.deepEqual(
    planVideoSeek({ currentVideoTime: 1, targetVideoTime: 2.5, isSeeking: true, coalesce: false }),
    { action: 'seek-now', target: 2.5 },
  )
  assert.deepEqual(
    planVideoSeek({ currentVideoTime: 2.5, targetVideoTime: 2.5, isSeeking: false, coalesce: true }),
    { action: 'already-at-target' },
  )
})

test('planSeekedFollowUp streams intermediate true frames while scrubbing and skips abandoned frames on release', () => {
  assert.deepEqual(
    planSeekedFollowUp({ currentVideoTime: 1.5, pendingSeekTarget: 2.5, activelyScrubbing: true }),
    { paintNow: true, nextSeekTarget: 2.5 },
  )
  assert.deepEqual(
    planSeekedFollowUp({ currentVideoTime: 1.5, pendingSeekTarget: 2.5, activelyScrubbing: false }),
    { paintNow: false, nextSeekTarget: 2.5 },
  )
  assert.deepEqual(
    planSeekedFollowUp({ currentVideoTime: 2.5, pendingSeekTarget: 2.5, activelyScrubbing: true }),
    { paintNow: true, nextSeekTarget: null },
  )
  assert.deepEqual(
    planSeekedFollowUp({ currentVideoTime: 2.5, pendingSeekTarget: null, activelyScrubbing: false }),
    { paintNow: true, nextSeekTarget: null },
  )
})

test('sourceToEditedTime and normalizeSourceToSegments track true playhead time across cuts and offsets', () => {
  const clips = [
    { start: 2, end: 4, sourceStart: 1, sourceEnd: 3 },
    { start: 4, end: 7, sourceStart: 6, sourceEnd: 9 },
  ]
  assert.equal(sourceToEditedTime(0.5, clips), 0)
  assert.equal(sourceToEditedTime(1, clips), 0)
  assert.equal(sourceToEditedTime(2.5, clips), 1.5)
  assert.equal(sourceToEditedTime(3, clips), 2)
  assert.equal(sourceToEditedTime(4.5, clips), 2)
  assert.equal(sourceToEditedTime(6, clips), 2)
  assert.equal(sourceToEditedTime(7.5, clips), 3.5)
  assert.equal(sourceToEditedTime(9, clips), 5)
  assert.equal(sourceToEditedTime(12, clips), 5)

  assert.equal(normalizeSourceToSegments(0.2, clips), 1)
  assert.equal(normalizeSourceToSegments(2.5, clips), 2.5)
  assert.equal(normalizeSourceToSegments(3, clips), 6)
  assert.equal(normalizeSourceToSegments(4.5, clips), 6)
  assert.equal(normalizeSourceToSegments(9, clips), 9)
  assert.equal(normalizeSourceToSegments(11, clips), 9)
})

test('timelineDurationForEdited shrinks when cuts reduce kept duration', () => {
  assert.equal(timelineDurationForEdited(12, 0), 42)
  assert.equal(timelineDurationForEdited(8, 2, 15, 0), 45)
  assert.equal(timelineDurationForEdited(0, 0), 30)
})
