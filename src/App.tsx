import { useCallback, useEffect, useEffectEvent, useMemo, useRef, useState } from 'react'
import type { PointerEvent as ReactPointerEvent } from 'react'
import { convertFileSrc, invoke } from '@tauri-apps/api/core'
import { emit, listen } from '@tauri-apps/api/event'
import { getCurrentWindow } from '@tauri-apps/api/window'
import { disable, enable, isEnabled } from '@tauri-apps/plugin-autostart'
import { register, unregisterAll } from '@tauri-apps/plugin-global-shortcut'
import { isPermissionGranted, requestPermission, sendNotification } from '@tauri-apps/plugin-notification'
import { relaunch } from '@tauri-apps/plugin-process'
import { check } from '@tauri-apps/plugin-updater'
import {
  Aperture,
  Camera,
  ChevronLeft,
  ChevronRight,
  Copy,
  Crop as CropIcon,
  Film,
  FolderOpen,
  Gauge,
  Clipboard,
  Link2,
  Magnet,
  Maximize2,
  Minimize2,
  Minus,
  Pause,
  Play,
  Plus,
  Radio,
  RotateCcw,
  Scissors,
  Settings2,
  SkipBack,
  SkipForward,
  Square,
  Timer,
  Upload,
  Redo2,
  Trash2,
  Undo2,
} from 'lucide-react'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card'
import { Input } from '@/components/ui/input'
import { Label } from '@/components/ui/label'
import { Select } from '@/components/ui/select'
import { Separator } from '@/components/ui/separator'
import type { AppSettings, BenchmarkResult, Crop, CutRange, ExportRequest, ExportResult, PreviewCache, RecordingRequest, VideoInfo } from '@/types'
import type { WaveformPeakData } from '@/types'
import {
  actualSizeViewer,
  canvasBackingSize,
  clampCropRect,
  fitViewer,
  initialViewerView,
  panViewer,
  resizeViewer,
  resizeRectWithLockedAspect,
  sourceRectToViewport,
  viewportToSource,
  zoomViewerAt,
} from '@/viewerGeometry'
import type { ViewerPoint, ViewerSize, ViewerView } from '@/viewerGeometry'
import { adjacentSourceFrame, cachedPreviewIndex, isCurrentFrameRequest, isFrameAtTime } from '@/viewerMedia'
import { runExportJob } from '@/exportJob'
import type { ExportOperation } from '@/exportJob'

const defaultSettings: AppSettings = {
  saveFolder: '',
  ffmpegPath: 'ffmpeg',
  frameRate: 30,
  captureBackend: 'desktopDuplication',
  maxMegabytes: 9.8,
  sizeCapEnabled: true,
  qualityTargetKbps: 10000,
  exportEncoderKey: 'x264-medium',
  exportBitrateScale: 1,
  audioGainDb: 0,
  unsupportedEncoderKeys: [],
  encoderBenchmarks: [],
  includeVideo: true,
  includeAudio: true,
  bestAudio: true,
  audioQuality: 'best',
  audioDeviceName: '',
  startWithWindows: true,
  startHiddenInTray: true,
  recordHotkey: 'Super+Shift+R',
  resetHotkey: 'Super+Shift+4',
  githubRepositoryUrl: 'https://github.com/daveranan/clipper',
}

const encoders = [
  ['x264-medium', 'H.264 Medium'],
  ['x264-slow', 'H.264 Slow'],
  ['x264-veryslow', 'H.264 Veryslow'],
  ['x265-medium', 'H.265 Medium'],
  ['x265-slow', 'H.265 Slow'],
  ['h264-nvenc-fast', 'H.264 NVENC Fast'],
  ['h264-nvenc', 'H.264 NVENC Quality'],
  ['h264-nvenc-max', 'H.264 NVENC Max'],
  ['hevc-nvenc-fast', 'H.265 NVENC Fast'],
  ['hevc-nvenc', 'H.265 NVENC Quality'],
  ['hevc-nvenc-max', 'H.265 NVENC Max'],
] as const

type TimelineDragState =
  | { mode: 'seek'; element: HTMLElement }
  | { mode: 'trim-start' | 'trim-end'; element: HTMLElement; track: TimelineTrack; timelineAnchor: number; sourceAnchor: number; timelineOffsetAnchor: number }
  | { mode: 'cut-start' | 'cut-end'; element: HTMLElement; cutIndex: number; track: TimelineTrack; timelineAnchor: number; sourceAnchor: number }
  | { mode: 'pan'; element: HTMLElement; startX: number; startTimelineStart: number }
  | { mode: 'marquee'; element: HTMLElement; startX: number; startY: number; currentX: number; currentY: number }

type TimelineDragInput =
  | { mode: 'seek' }
  | { mode: 'trim-start' | 'trim-end'; track: TimelineTrack; sourceAnchor: number }
  | { mode: 'cut-start' | 'cut-end'; cutIndex: number; track: TimelineTrack; sourceAnchor: number }
  | { mode: 'pan'; startX: number; startTimelineStart: number }

type TimelineTrack = 'video' | 'audio'
type ClipSelection = { track: TimelineTrack; index: number }
type SelectedClip = ClipSelection | null
type DisplayClip = CutRange & { sourceStart: number; sourceEnd: number }
type MarqueeSelection = { startX: number; startY: number; currentX: number; currentY: number } | null
type EditSnapshot = {
  trimStart: number
  trimEnd: number
  timelineOffset: number
  audioTrimStart: number
  audioTrimEnd: number
  audioTimelineOffset: number
  cuts: CutRange[]
  audioCuts: CutRange[]
  crop: Crop
  outputWidth: number
  outputHeight: number
  autoFit720: boolean
  audioGainDb: number
}

function App() {
  if (new URLSearchParams(window.location.search).get('selector') === '1') {
    return <RegionSelector />
  }

  return <MainApp />
}

function MainApp() {
  const [settings, setSettings] = useState<AppSettings>(defaultSettings)
  const [clip, setClip] = useState<VideoInfo | null>(null)
  const [preview, setPreview] = useState<PreviewCache | null>(null)
  const [playbackPath, setPlaybackPath] = useState<string | null>(null)
  const [exactFrame, setExactFrame] = useState<string | null>(null)
  const [status, setStatus] = useState('Ready')
  const [isPreparing, setIsPreparing] = useState(false)
  const [isExporting, setIsExporting] = useState(false)
  const [isStoppingExport, setIsStoppingExport] = useState(false)
  const [isCopyingFrame, setIsCopyingFrame] = useState(false)
  const exportOperationRef = useRef<ExportOperation | null>(null)
  const copyingFrameRef = useRef(false)
  const [isCheckingUpdate, setIsCheckingUpdate] = useState(false)
  const [isRecording, setIsRecording] = useState(false)
  const [isPlaying, setIsPlaying] = useState(false)
  const [audioDevices, setAudioDevices] = useState<string[]>([])
  const [benchmarkResults, setBenchmarkResults] = useState<BenchmarkResult[]>([])
  const [lastExport, setLastExport] = useState<ExportResult | null>(null)
  const [waveform, setWaveform] = useState<WaveformPeakData | null>(null)
  const [currentTime, setCurrentTime] = useState(0)
  const [trimStart, setTrimStart] = useState(0)
  const [trimEnd, setTrimEnd] = useState(0)
  const [audioTrimStart, setAudioTrimStart] = useState(0)
  const [audioTrimEnd, setAudioTrimEnd] = useState(0)
  const [crop, setCrop] = useState<Crop>({ x: 0, y: 0, width: 1280, height: 720 })
  const [outputWidth, setOutputWidth] = useState(1280)
  const [outputHeight, setOutputHeight] = useState(720)
  const [autoFit720, setAutoFit720] = useState(false)
  const [cuts, setCuts] = useState<CutRange[]>([])
  const [audioCuts, setAudioCuts] = useState<CutRange[]>([])
  const [pendingCutStart, setPendingCutStart] = useState<number | null>(null)
  const [recordingRegion, setRecordingRegion] = useState({ x: 0, y: 0, width: 1280, height: 720 })
  const [timelineZoom, setTimelineZoom] = useState(1)
  const [timelineStart, setTimelineStart] = useState(0)
  const [timelineOffset, setTimelineOffset] = useState(0)
  const [audioTimelineOffset, setAudioTimelineOffset] = useState(0)
  const [selectedClip, setSelectedClip] = useState<SelectedClip>(null)
  const [selectedClips, setSelectedClips] = useState<ClipSelection[]>([])
  const [marqueeSelection, setMarqueeSelection] = useState<MarqueeSelection>(null)
  const [isTimelinePanning, setIsTimelinePanning] = useState(false)
  const [, setIsTimelinePreviewing] = useState(false)
  const [isCScrubbing, setIsCScrubbing] = useState(false)
  const [snappingEnabled, setSnappingEnabled] = useState(true)
  const [syncLinked, setSyncLinked] = useState(true)
  const [settingsOpen, setSettingsOpen] = useState(false)
  const [isGeneratingWaveform, setIsGeneratingWaveform] = useState(false)
  const frameRequestRef = useRef(0)
  const clipEpochRef = useRef(0)
  const currentTimeRef = useRef(0)
  const timelineDragRef = useRef<TimelineDragState | null>(null)
  const timelineSurfaceRef = useRef<HTMLDivElement | null>(null)
  const cScrubModeRef = useRef(false)
  const exactFrameAfterDragRef = useRef(false)
  const exactFrameAfterStepRef = useRef(false)
  const audioMutedForSourceRef = useRef<(sourceTime: number) => boolean>(() => false)
  const editHistoryRef = useRef<{ past: EditSnapshot[]; future: EditSnapshot[]; activeLabel: string | null }>({ past: [], future: [], activeLabel: null })
  const [historyState, setHistoryState] = useState({ canUndo: false, canRedo: false })
  const playbackRef = useRef<HTMLVideoElement | null>(null)
  const viewerCanvasRef = useRef<HTMLCanvasElement | null>(null)
  const viewerSurfaceRef = useRef<HTMLDivElement | null>(null)
  const viewerStillRef = useRef<HTMLImageElement | null>(null)
  const [viewerMediaSize, setViewerMediaSize] = useState<{ width: number; height: number } | null>(null)
  const [viewerSize, setViewerSize] = useState<{ width: number; height: number } | null>(null)
  const [viewerView, setViewerView] = useState<ViewerView>(initialViewerView)
  const viewerViewRef = useRef<ViewerView>(initialViewerView)
  const previousViewerSizeRef = useRef<ViewerSize | null>(null)
  const viewerClipIdRef = useRef<string | null>(null)
  const viewerPanRef = useRef<{ pointerId: number; x: number; y: number } | null>(null)
  const [isViewerPanning, setIsViewerPanning] = useState(false)
  const [isViewerFullscreen, setIsViewerFullscreen] = useState(false)
  const [viewerDrawTick, setViewerDrawTick] = useState(0)
  const bumpViewerDraw = useCallback(() => setViewerDrawTick((value) => value + 1), [])
  const viewerHasContentRef = useRef(false)
  const toggleRecordingRef = useRef<() => void>(() => undefined)
  const resetRecordingRef = useRef<() => void>(() => undefined)
  const startRegionRecordingRef = useRef<(region: Omit<RecordingRequest, 'settings'>) => void>(() => undefined)

  useEffect(() => {
    if (!canUseTauri()) {
      setStatus('Browser preview')
      return
    }

    Promise.all([tauriInvoke<AppSettings>('load_settings'), isEnabled().catch(() => false)])
      .then(async ([loaded, startWithWindowsEnabled]) => {
        const nextSettings = { ...defaultSettings, ...loaded }
        try {
          if (nextSettings.startWithWindows) {
            if (startWithWindowsEnabled) {
              await disable()
            }
            await enable()
          } else if (!nextSettings.startWithWindows && startWithWindowsEnabled) {
            await disable()
          }
        } catch (error) {
          setStatus(`Startup unavailable: ${shortError(error)}`)
        }
        setSettings(nextSettings)
      })
      .catch((error) => setStatus(shortError(error)))
  }, [])

  useEffect(() => {
    currentTimeRef.current = currentTime
  }, [currentTime])

  const clipName = clip ? fileName(clip.originalPath) : 'No clip'
  const keptSeconds = useMemo(() => {
    const raw = Math.max(0, trimEnd - trimStart)
    const removed = cuts.reduce((sum, cut) => sum + Math.max(0, Math.min(cut.end, trimEnd) - Math.max(cut.start, trimStart)), 0)
    return Math.max(0, raw - removed)
  }, [cuts, trimEnd, trimStart])

  const audioKeptSeconds = useMemo(() => {
    const raw = Math.max(0, audioTrimEnd - audioTrimStart)
    const removed = audioCuts.reduce((sum, cut) => sum + Math.max(0, Math.min(cut.end, audioTrimEnd) - Math.max(cut.start, audioTrimStart)), 0)
    return Math.max(0, raw - removed)
  }, [audioCuts, audioTrimEnd, audioTrimStart])

  useEffect(() => {
    const duration = timelineDurationFor(clip?.durationSeconds ?? 1, keptSeconds, timelineOffset, audioKeptSeconds, audioTimelineOffset)
    const span = duration / Math.max(1, timelineZoom)
    setTimelineStart((value) => clamp(value, 0, Math.max(0, duration - span)))
  }, [audioKeptSeconds, audioTimelineOffset, clip?.durationSeconds, keptSeconds, timelineOffset, timelineZoom])

  useEffect(() => {
    if (!isPlaying || !clip) {
      playbackRef.current?.pause()
      return
    }

    const media = playbackRef.current
    const startTime = currentTimeRef.current
    if (media) {
      media.muted = audioMutedForSourceRef.current(startTime)
      media.volume = 1
      if (Math.abs(media.currentTime - startTime) > 0.15) {
        media.currentTime = startTime
      }
      media.play().catch((error) => {
        if (!isInterruptedPlaybackError(error)) {
          setStatus(shortError(error))
        }
      })
    }

    let frame = 0
    const startedAt = performance.now()
    let lastUiUpdateAt = 0
    const tick = () => {
      const now = performance.now()
      const elapsed = (now - startedAt) / 1000
      const previous = currentTimeRef.current
      const rawNext = media && !Number.isNaN(media.currentTime) ? media.currentTime : startTime + elapsed
      const next = nextPlayableSourceTime(previous, rawNext, cuts, trimStart, trimEnd)
      if (media && Math.abs(media.currentTime - next) > 0.04) {
        media.currentTime = next
      }
      if (media) {
        media.muted = audioMutedForSourceRef.current(next)
      }
      if (next >= trimEnd) {
        currentTimeRef.current = trimEnd
        setCurrentTime(trimEnd)
        setIsPlaying(false)
        media?.pause()
        return
      }
      currentTimeRef.current = next
      if (now - lastUiUpdateAt >= 1000 / 30) {
        lastUiUpdateAt = now
        setCurrentTime(next)
      }
      frame = requestAnimationFrame(tick)
    }
    frame = requestAnimationFrame(tick)
    return () => {
      cancelAnimationFrame(frame)
      media?.pause()
    }
  }, [clip, cuts, isPlaying, trimEnd, trimStart])

  const previewSrc = useMemo(() => {
    // C-scrub keeps the still mounted: it resolves to the preview-cache frame
    // for the current time so the canvas can follow the cursor immediately.
    // Only playback nulls it (the video is the live source while playing).
    if (isPlaying && !exactFrame) {
      return null
    }
    if (exactFrame) {
      return convertFileSrc(exactFrame)
    }
    if (!preview || preview.frames.length === 0) {
      return null
    }
    const index = cachedPreviewIndex(currentTime, preview.fps, preview.frames.length)
    if (index === null) {
      return null
    }
    return convertFileSrc(preview.frames[index])
  }, [currentTime, exactFrame, isPlaying, preview])
  const clipMediaSrc = useMemo(() => (playbackPath ? convertFileSrc(playbackPath) : null), [playbackPath])

  useEffect(() => {
    const media = playbackRef.current
    if (!media || !clipMediaSrc) {
      return
    }
    media.load()
    media.currentTime = currentTimeRef.current
  }, [clipMediaSrc])

  const viewerSourceSize = useMemo(
    () => clip ? { width: clip.width, height: clip.height } : viewerMediaSize,
    [clip, viewerMediaSize],
  )

  useEffect(() => {
    viewerViewRef.current = viewerView
  }, [viewerView])

  useEffect(() => {
    const surface = viewerSurfaceRef.current
    if (!surface) {
      return
    }
    const measure = () => setViewerSize({ width: surface.clientWidth, height: surface.clientHeight })
    measure()
    if (typeof ResizeObserver === 'undefined') {
      return
    }
    const observer = new ResizeObserver(measure)
    observer.observe(surface)
    return () => observer.disconnect()
  }, [])

  useEffect(() => {
    if (!viewerSize || !viewerSourceSize) {
      return
    }
    const previousSize = previousViewerSizeRef.current ?? viewerSize
    const clipId = clip?.id ?? null
    const nextView = viewerClipIdRef.current !== clipId
      ? fitViewer(viewerSize, viewerSourceSize)
      : resizeViewer(viewerViewRef.current, previousSize, viewerSize, viewerSourceSize)
    viewerClipIdRef.current = clipId
    previousViewerSizeRef.current = viewerSize
    viewerViewRef.current = nextView
    setViewerView(nextView)
  }, [clip?.id, viewerSize, viewerSourceSize])

  useEffect(() => {
    const canvas = viewerCanvasRef.current
    if (!canvas || !viewerSize || viewerSize.width <= 0 || viewerSize.height <= 0) {
      return
    }
    const { width, height } = canvasBackingSize(viewerSize, window.devicePixelRatio)
    if (canvas.width !== width || canvas.height !== height) {
      canvas.width = width
      canvas.height = height
      viewerHasContentRef.current = false
    }
    bumpViewerDraw()
  }, [bumpViewerDraw, viewerSize])

  useEffect(() => {
    setViewerMediaSize(null)
    // New source: the old painted frame belongs to the previous clip.
    viewerHasContentRef.current = false
  }, [clipMediaSrc])

  const drawViewerFrame = useCallback(() => {
    const canvas = viewerCanvasRef.current
    const sourceSize = viewerSourceSize
    const viewportSize = viewerSize
    if (!canvas || !sourceSize || !viewportSize || canvas.width === 0 || canvas.height === 0) {
      return
    }
    const context = canvas.getContext('2d')
    if (!context) {
      return
    }
    const video = playbackRef.current
    const still = viewerStillRef.current
    const videoDrawable = Boolean(video && video.readyState >= 2 && video.videoWidth > 0)
    const stillDrawable = Boolean(still && still.complete && still.naturalWidth > 0)
    // True once the video has settled on the requested time (seek finished,
    // no seek in flight). Its frame is native resolution, so a settled video
    // is the highest-quality paused preview available.
    const videoCurrent =
      videoDrawable &&
      video !== null &&
      !video.seeking &&
      isFrameAtTime(video.currentTime, currentTimeRef.current, settings.frameRate)
    const paint = (source: CanvasImageSource) => {
      const view = viewerViewRef.current
      const backingScaleX = canvas.width / Math.max(1, viewportSize.width)
      const backingScaleY = canvas.height / Math.max(1, viewportSize.height)
      context.setTransform(1, 0, 0, 1, 0, 0)
      context.clearRect(0, 0, canvas.width, canvas.height)
      context.imageSmoothingEnabled = true
      context.imageSmoothingQuality = 'high'
      context.setTransform(
        backingScaleX * view.scale,
        0,
        0,
        backingScaleY * view.scale,
        backingScaleX * view.offsetX,
        backingScaleY * view.offsetY,
      )
      context.drawImage(source, 0, 0, sourceSize.width, sourceSize.height)
      viewerHasContentRef.current = true
    }
    if (isPlaying) {
      if (video && videoDrawable) {
        paint(video)
      }
      // else: video not ready yet — keep the last painted frame
    } else if (isCScrubbing) {
      // C-scrub follows the cursor with the preview-cache still; the video
      // lags behind in-flight seeks, so it is never drawn while C-scrubbing.
      if (still && stillDrawable) {
        paint(still)
      }
      // else: next cache frame still loading — keep the last painted frame
    } else if (exactFrame && stillDrawable) {
      if (still) {
        paint(still)
      }
    } else if (videoCurrent) {
      if (video) {
        paint(video)
      }
    } else if (stillDrawable) {
      if (still) {
        paint(still)
      }
    } else if (videoDrawable && !viewerHasContentRef.current) {
      // Preparing state: the video is the only media and nothing is painted yet.
      if (video) {
        paint(video)
      }
    } else if (!viewerHasContentRef.current) {
      context.setTransform(1, 0, 0, 1, 0, 0)
      context.clearRect(0, 0, canvas.width, canvas.height)
    }
    // else: a new still is loading — keep the last painted frame (no flicker)
  }, [exactFrame, isCScrubbing, isPlaying, settings.frameRate, viewerSize, viewerSourceSize])

  useEffect(() => {
    const media = playbackRef.current
    if (!isPlaying || !media) {
      return
    }
    let cancelled = false
    let animationFrame = 0
    let videoFrame = 0
    if (typeof media.requestVideoFrameCallback === 'function') {
      const tick = () => {
        if (cancelled) {
          return
        }
        drawViewerFrame()
        videoFrame = media.requestVideoFrameCallback(tick)
      }
      videoFrame = media.requestVideoFrameCallback(tick)
    } else {
      const tick = () => {
        if (cancelled) {
          return
        }
        drawViewerFrame()
        animationFrame = requestAnimationFrame(tick)
      }
      animationFrame = requestAnimationFrame(tick)
    }
    return () => {
      cancelled = true
      cancelAnimationFrame(animationFrame)
      if (videoFrame && typeof media.cancelVideoFrameCallback === 'function') {
        media.cancelVideoFrameCallback(videoFrame)
      }
    }
  }, [drawViewerFrame, isPlaying])

  useEffect(() => {
    if (isPlaying) {
      return
    }
    const frame = requestAnimationFrame(drawViewerFrame)
    return () => cancelAnimationFrame(frame)
  }, [clipMediaSrc, drawViewerFrame, isCScrubbing, isPlaying, previewSrc, viewerDrawTick, viewerMediaSize, viewerSize, viewerView])

  const commitViewerView = useCallback((update: (current: ViewerView) => ViewerView) => {
    setViewerView((current) => {
      const next = update(current)
      viewerViewRef.current = next
      return next
    })
  }, [])

  const zoomViewerBy = useCallback((factor: number, anchor?: ViewerPoint) => {
    if (!viewerSize || !viewerSourceSize) {
      return
    }
    const zoomAnchor = anchor ?? { x: viewerSize.width / 2, y: viewerSize.height / 2 }
    commitViewerView((current) => zoomViewerAt(current, zoomAnchor, current.scale * factor, viewerSize, viewerSourceSize))
  }, [commitViewerView, viewerSize, viewerSourceSize])

  const resetViewer = useCallback(() => {
    if (!viewerSize || !viewerSourceSize) {
      return
    }
    commitViewerView(() => fitViewer(viewerSize, viewerSourceSize))
  }, [commitViewerView, viewerSize, viewerSourceSize])

  const showViewerActualSize = useCallback(() => {
    if (!viewerSize || !viewerSourceSize) {
      return
    }
    commitViewerView(() => actualSizeViewer(viewerSize, viewerSourceSize))
  }, [commitViewerView, viewerSize, viewerSourceSize])

  const toggleViewerFullscreen = useCallback(() => {
    if (!viewerSourceSize) {
      return
    }
    const next = !isViewerFullscreen
    resetViewer()
    setIsViewerFullscreen(next)
    if (canUseTauri()) {
      void getCurrentWindow().setFullscreen(next).catch(() => undefined)
    }
  }, [isViewerFullscreen, resetViewer, viewerSourceSize])

  const exitViewerFullscreen = useCallback(() => {
    if (!isViewerFullscreen) {
      return
    }
    resetViewer()
    setIsViewerFullscreen(false)
    if (canUseTauri()) {
      void getCurrentWindow().setFullscreen(false).catch(() => undefined)
    }
  }, [isViewerFullscreen, resetViewer])

  useEffect(() => {
    const surface = viewerSurfaceRef.current
    if (!surface || !viewerSize || !viewerSourceSize) {
      return
    }
    const onWheel = (event: WheelEvent) => {
      event.preventDefault()
      const rect = surface.getBoundingClientRect()
      const anchor = { x: event.clientX - rect.left, y: event.clientY - rect.top }
      const deltaPixels = event.deltaY * (event.deltaMode === WheelEvent.DOM_DELTA_LINE ? 16 : event.deltaMode === WheelEvent.DOM_DELTA_PAGE ? rect.height : 1)
      const factor = clamp(Math.exp(-deltaPixels * 0.0015), 0.8, 1.25)
      zoomViewerBy(factor, anchor)
    }
    const onAuxClick = (event: MouseEvent) => {
      if (event.button === 1) {
        event.preventDefault()
      }
    }
    surface.addEventListener('wheel', onWheel, { passive: false })
    surface.addEventListener('auxclick', onAuxClick)
    return () => {
      surface.removeEventListener('wheel', onWheel)
      surface.removeEventListener('auxclick', onAuxClick)
    }
  }, [viewerSize, viewerSourceSize, zoomViewerBy])

  const beginViewerPan = (event: ReactPointerEvent<HTMLDivElement>) => {
    if (event.button !== 1 || !viewerSourceSize) {
      return
    }
    event.preventDefault()
    viewerPanRef.current = { pointerId: event.pointerId, x: event.clientX, y: event.clientY }
    setIsViewerPanning(true)
    event.currentTarget.setPointerCapture(event.pointerId)
  }

  const moveViewerPan = (event: ReactPointerEvent<HTMLDivElement>) => {
    const pan = viewerPanRef.current
    if (!pan || pan.pointerId !== event.pointerId) {
      return
    }
    const delta = { x: event.clientX - pan.x, y: event.clientY - pan.y }
    pan.x = event.clientX
    pan.y = event.clientY
    commitViewerView((current) => panViewer(current, delta))
  }

  const endViewerPan = (event: ReactPointerEvent<HTMLDivElement>) => {
    if (viewerPanRef.current?.pointerId !== event.pointerId) {
      return
    }
    viewerPanRef.current = null
    setIsViewerPanning(false)
    if (event.currentTarget.hasPointerCapture(event.pointerId)) {
      event.currentTarget.releasePointerCapture(event.pointerId)
    }
  }

  const qualityMaxSeconds = useMemo(() => {
    const audioKbps = settings.includeAudio
      ? settings.includeVideo
        ? settings.audioQuality === 'standard' ? 160 : 320
        : settings.audioQuality === 'best' ? 2304 : settings.audioQuality === 'optimized' ? 1536 : 1412
      : 0
    const pixels = Math.max(1, outputWidth * outputHeight)
    const scale = pixels / (1920 * 1080)
    const videoKbps = settings.includeVideo ? Math.max(1200, Math.min(settings.qualityTargetKbps, Math.round(settings.qualityTargetKbps * scale))) : 0
    return (settings.maxMegabytes * 8192 * 0.985) / Math.max(1, videoKbps + audioKbps)
  }, [outputHeight, outputWidth, settings.audioQuality, settings.includeAudio, settings.includeVideo, settings.maxMegabytes, settings.qualityTargetKbps])

  const timelineViewport = useMemo(() => {
    const duration = timelineDurationFor(clip?.durationSeconds || 1, keptSeconds, timelineOffset, audioKeptSeconds, audioTimelineOffset)
    const span = duration / Math.max(1, timelineZoom)
    const start = clamp(timelineStart, 0, Math.max(0, duration - span))
    return {
      start,
      end: start + span,
      duration,
      sourceDuration: clip?.durationSeconds ?? 0,
      span,
    }
  }, [audioKeptSeconds, audioTimelineOffset, clip?.durationSeconds, keptSeconds, timelineOffset, timelineStart, timelineZoom])

  const videoClipSegments = useMemo(() => {
    if (!clip) {
      return []
    }
    return buildDisplayClips(trimStart, trimEnd, cuts, timelineOffset)
  }, [clip, cuts, timelineOffset, trimEnd, trimStart])

  const audioClipSegments = useMemo(() => {
    if (!clip) {
      return []
    }
    return buildDisplayClips(audioTrimStart, audioTrimEnd, audioCuts, audioTimelineOffset)
  }, [audioCuts, audioTimelineOffset, audioTrimEnd, audioTrimStart, clip])

  useEffect(() => {
    audioMutedForSourceRef.current = (sourceTime: number) => {
      if (!settings.includeAudio) {
        return true
      }
      const timelineTime = sourceToTimelineTime(sourceTime, videoClipSegments)
      return !timelineHasClipAt(timelineTime, audioClipSegments)
    }
    if (playbackRef.current) {
      playbackRef.current.muted = audioMutedForSourceRef.current(currentTimeRef.current)
    }
  }, [audioClipSegments, currentTime, settings.includeAudio, videoClipSegments])

  const refreshHistoryState = () => {
    const history = editHistoryRef.current
    setHistoryState({ canUndo: history.past.length > 0, canRedo: history.future.length > 0 })
  }

  const { canUndo, canRedo } = historyState

  const clearClipSelection = () => {
    setSelectedClip(null)
    setSelectedClips([])
  }

  const selectClip = (selection: ClipSelection) => {
    setSelectedClip(selection)
    setSelectedClips([selection])
  }

  const currentEditSnapshot = (): EditSnapshot => ({
    trimStart,
    trimEnd,
    timelineOffset,
    audioTrimStart,
    audioTrimEnd,
    audioTimelineOffset,
    cuts: cloneCuts(cuts),
    audioCuts: cloneCuts(audioCuts),
    crop: { ...crop },
    outputWidth,
    outputHeight,
    autoFit720,
    audioGainDb: settings.audioGainDb,
  })

  const applyEditSnapshot = (snapshot: EditSnapshot) => {
    setTrimStart(snapshot.trimStart)
    setTrimEnd(snapshot.trimEnd)
    setTimelineOffset(snapshot.timelineOffset)
    setAudioTrimStart(snapshot.audioTrimStart)
    setAudioTrimEnd(snapshot.audioTrimEnd)
    setAudioTimelineOffset(snapshot.audioTimelineOffset)
    setCuts(cloneCuts(snapshot.cuts))
    setAudioCuts(cloneCuts(snapshot.audioCuts))
    setCrop({ ...snapshot.crop })
    setOutputWidth(snapshot.outputWidth)
    setOutputHeight(snapshot.outputHeight)
    setAutoFit720(snapshot.autoFit720)
    setSettings((value) => ({ ...value, audioGainDb: snapshot.audioGainDb }))
    clearClipSelection()
    setPendingCutStart(null)
    if (clip) {
      setCurrentTime((value) => clamp(value, snapshot.trimStart, snapshot.trimEnd))
    }
  }

  const beginEdit = (label: string) => {
    const history = editHistoryRef.current
    if (history.activeLabel === label) {
      return
    }
    const snapshot = currentEditSnapshot()
    const last = history.past[history.past.length - 1]
    if (!last || !editSnapshotsEqual(last, snapshot)) {
      history.past = [...history.past.slice(-79), snapshot]
    }
    history.future = []
    history.activeLabel = label
    refreshHistoryState()
  }

  const finishEdit = () => {
    editHistoryRef.current.activeLabel = null
  }

  const undoEdit = () => {
    const history = editHistoryRef.current
    const previous = history.past[history.past.length - 1]
    if (!previous) {
      return
    }
    history.past = history.past.slice(0, -1)
    history.future = [currentEditSnapshot(), ...history.future].slice(0, 80)
    history.activeLabel = null
    applyEditSnapshot(previous)
    refreshHistoryState()
    setStatus('Undo')
  }

  const redoEdit = () => {
    const history = editHistoryRef.current
    const next = history.future[0]
    if (!next) {
      return
    }
    history.future = history.future.slice(1)
    history.past = [...history.past.slice(-79), currentEditSnapshot()]
    history.activeLabel = null
    applyEditSnapshot(next)
    refreshHistoryState()
    setStatus('Redo')
  }

  const clearExactFrame = () => {
    frameRequestRef.current += 1
    setExactFrame(null)
  }

  const loadClip = useCallback(
    async (nextClip: VideoInfo) => {
      const clipEpoch = ++clipEpochRef.current
      frameRequestRef.current += 1
      viewerClipIdRef.current = null
      setClip(nextClip)
      setPreview(null)
      setPlaybackPath(null)
      setWaveform(null)
      setExactFrame(null)
      setLastExport(null)
      setCurrentTime(0)
      setTrimStart(0)
      setTrimEnd(nextClip.durationSeconds)
      setTimelineOffset(0)
      setAudioTrimStart(0)
      setAudioTrimEnd(nextClip.durationSeconds)
      setAudioTimelineOffset(0)
      setCuts([])
      setAudioCuts([])
      setPendingCutStart(null)
      clearClipSelection()
      setCrop({ x: 0, y: 0, width: nextClip.width, height: nextClip.height })
      setOutputWidth(makeEven(nextClip.width))
      setOutputHeight(makeEven(nextClip.height))
      setAutoFit720(false)
      setTimelineZoom(1)
      setTimelineStart(0)
      editHistoryRef.current = { past: [], future: [], activeLabel: null }
      refreshHistoryState()
      setStatus('Preparing preview cache')
      setIsPreparing(true)
      setIsGeneratingWaveform(true)
      try {
        const [playbackResult, cacheResult, waveformResult] = await Promise.allSettled([
          tauriInvoke<string>('prepare_playback_source', {
            inputPath: nextClip.path,
            settings,
          }),
          tauriInvoke<PreviewCache>('prepare_preview_cache', {
            inputPath: nextClip.path,
            fps: Math.min(settings.frameRate, 12),
            maxSeconds: Math.min(nextClip.durationSeconds, 180),
            settings,
          }),
          tauriInvoke<WaveformPeakData>('generate_waveform', {
            inputPath: nextClip.path,
            points: clamp(Math.ceil(nextClip.durationSeconds * 600), 12000, 65536),
            settings,
          }),
        ])
        if (clipEpoch !== clipEpochRef.current) {
          return
        }
        if (playbackResult.status === 'fulfilled') {
          setPlaybackPath(playbackResult.value)
        } else {
          setStatus(shortError(playbackResult.reason))
        }
        if (cacheResult.status === 'fulfilled') {
          setPreview(cacheResult.value)
        } else {
          setStatus(shortError(cacheResult.reason))
        }
        if (waveformResult.status === 'fulfilled') {
          setWaveform(waveformResult.value)
        }
        setStatus('Clip ready')
      } catch (error) {
        if (clipEpoch === clipEpochRef.current) {
          setStatus(shortError(error))
        }
      } finally {
        if (clipEpoch === clipEpochRef.current) {
          setIsPreparing(false)
          setIsGeneratingWaveform(false)
        }
      }
    },
    [settings],
  )

  const showExactFrame = useCallback(
    async (time: number) => {
      if (!clip) {
        return
      }
      const clipEpoch = clipEpochRef.current
      const requestId = ++frameRequestRef.current
      const request = { clipEpoch, requestId, secondsAt: time }
      setExactFrame(null)
      try {
        const path = await tauriInvoke<string>('extract_exact_frame', {
          inputPath: clip.path,
          secondsAt: time,
          settings,
        })
        if (isCurrentFrameRequest(request, clipEpochRef.current, frameRequestRef.current, currentTimeRef.current, settings.frameRate)) {
          setExactFrame(path)
        }
      } catch (error) {
        if (isCurrentFrameRequest(request, clipEpochRef.current, frameRequestRef.current, currentTimeRef.current, settings.frameRate)) {
          setStatus(shortError(error))
        }
      }
    },
    [clip, settings],
  )
  const requestExactFrameFromKey = useEffectEvent((time: number) => {
    void showExactFrame(time)
  })

  const openVideo = async () => {
    setStatus('Opening video')
    try {
      const nextClip = await tauriInvoke<VideoInfo | null>('open_video_dialog', { settings })
      if (nextClip) {
        await loadClip(nextClip)
      } else {
        setStatus('Ready')
      }
    } catch (error) {
      setStatus(shortError(error))
    }
  }

  const saveSettings = async () => {
    try {
      if (settings.startWithWindows) {
        await disable()
        await enable()
      } else {
        await disable()
      }
      await tauriInvoke('save_settings', { settings })
      setStatus('Settings saved')
    } catch (error) {
      setStatus(shortError(error))
    }
  }

  const browseSaveFolder = async () => {
    try {
      const saveFolder = await tauriInvoke<string | null>('choose_save_folder', { current: settings.saveFolder })
      if (saveFolder) {
        setSettings((value) => ({ ...value, saveFolder }))
      }
    } catch (error) {
      setStatus(shortError(error))
    }
  }

  const browseFfmpeg = async () => {
    try {
      const ffmpegPath = await tauriInvoke<string | null>('choose_ffmpeg_path')
      if (ffmpegPath) {
        setSettings((value) => ({ ...value, ffmpegPath }))
      }
    } catch (error) {
      setStatus(shortError(error))
    }
  }

  const scanAudioDevices = async () => {
    try {
      const devices = await tauriInvoke<string[]>('list_audio_devices', { settings })
      setAudioDevices(devices)
      setStatus(devices.length > 0 ? `Found ${devices.length} audio devices` : 'No FFmpeg dshow audio devices found')
    } catch (error) {
      setStatus(shortError(error))
    }
  }

  const exportClip = async () => {
    if (!clip || exportOperationRef.current) {
      return
    }
    const operation: ExportOperation = { jobId: null, stopRequested: false }
    exportOperationRef.current = operation
    setIsExporting(true)
    setIsStoppingExport(false)
    setStatus('Exporting')
    try {
      await nextPaint()
      const extension = settings.includeVideo ? 'mp4' : 'wav'
      const outputPath = joinPath(settings.saveFolder, `clip-${dateStamp()}.${extension}`)
      const request: ExportRequest = {
        inputPath: clip.path,
        outputPath,
        start: trimStart,
        end: trimEnd,
        audioStart: audioTrimStart,
        audioEnd: audioTrimEnd,
        crop,
        outputWidth,
        outputHeight,
        autoFit720,
        cuts,
        audioCuts,
        settings,
      }
      const result = await runExportJob<ExportResult>(tauriInvoke, operation, 'export_clip', request)
      setLastExport(result)
      setStatus(`Export ready: ${fileName(result.path)} (${formatBytes(result.bytes)}, ${result.seconds.toFixed(1)}s)`)
    } catch (error) {
      setStatus(shortError(error))
    } finally {
      exportOperationRef.current = null
      setIsExporting(false)
      setIsStoppingExport(false)
    }
  }

  const stopExport = async () => {
    const operation = exportOperationRef.current
    if (!operation || operation.stopRequested) return
    operation.stopRequested = true
    setIsStoppingExport(true)
    setStatus('Stopping export')
    if (!operation.jobId) return
    try {
      await tauriInvoke('cancel_export', { jobId: operation.jobId })
    } catch (error) {
      if (exportOperationRef.current === operation) {
        operation.stopRequested = false
        setIsStoppingExport(false)
        setStatus(shortError(error))
      }
    }
  }

  const copyVideoFrame = async () => {
    if (!clip || copyingFrameRef.current) return
    copyingFrameRef.current = true
    setIsCopyingFrame(true)
    const secondsAt = currentTimeRef.current
    playbackRef.current?.pause()
    setIsPlaying(false)
    setCurrentTime(secondsAt)
    setStatus('Copying full-resolution frame')
    try {
      const result = await tauriInvoke<{ width: number; height: number }>('copy_video_frame', {
        inputPath: clip.path,
        secondsAt,
        settings,
      })
      setStatus(`Screenshot copied to clipboard (${result.width}×${result.height})`)
    } catch (error) {
      setStatus(shortError(error))
    } finally {
      copyingFrameRef.current = false
      setIsCopyingFrame(false)
    }
  }

  const benchmarkEncoders = async () => {
    if (!clip || exportOperationRef.current) {
      return
    }
    const operation: ExportOperation = { jobId: null, stopRequested: false }
    exportOperationRef.current = operation
    setIsExporting(true)
    setIsStoppingExport(false)
    setStatus('Benchmarking encoders')
    try {
      await nextPaint()
      const outputPath = await tauriInvoke<string | null>('choose_export_path', {
        saveFolder: settings.saveFolder,
        fileName: `benchmark-${dateStamp()}.mp4`,
      })
      if (!outputPath) {
        setStatus('Benchmark canceled')
        return
      }
      const request: ExportRequest = {
        inputPath: clip.path,
        outputPath,
        start: trimStart,
        end: trimEnd,
        audioStart: audioTrimStart,
        audioEnd: audioTrimEnd,
        crop,
        outputWidth,
        outputHeight,
        autoFit720,
        cuts,
        audioCuts,
        settings,
      }
      const results = await runExportJob<BenchmarkResult[]>(tauriInvoke, operation, 'benchmark_encoders', request)
      setBenchmarkResults(results)
      const successes = results.filter((result) => result.success)
      const unsupported = results.filter((result) => !result.success).map((result) => result.encoderKey)
      const encoderBenchmarks = successes.map((result) => ({
        encoderKey: result.encoderKey,
        encoderLabel: result.encoderLabel,
        bytes: result.bytes,
        seconds: result.seconds,
        testedAt: new Date().toISOString(),
      }))
      setSettings((value) => ({
        ...value,
        encoderBenchmarks,
        unsupportedEncoderKeys: Array.from(new Set([...value.unsupportedEncoderKeys, ...unsupported])),
      }))
      const fastest = [...successes].sort((a, b) => a.seconds - b.seconds)[0]
      setStatus(fastest ? `Benchmark complete. Fastest: ${fastest.encoderLabel}` : 'Benchmark complete')
    } catch (error) {
      setStatus(shortError(error))
    } finally {
      exportOperationRef.current = null
      setIsExporting(false)
      setIsStoppingExport(false)
    }
  }

  const focusAppWindow = async () => {
    if (canUseTauri()) {
      try {
        await tauriInvoke('focus_main_window')
        return
      } catch {
        // Fall back to the webview handle below.
      }
    }
    const appWindow = getCurrentWindow()
    await appWindow.show()
    await appWindow.setFocus()
  }

  const toggleRecording = async () => {
    try {
      if (isRecording) {
        setStatus('Stopping recording')
        const nextClip = await tauriInvoke<VideoInfo>('stop_recording', { settings })
        setIsRecording(false)
        await focusAppWindow()
        await loadClip(nextClip)
        await focusAppWindow()
        return
      }
      setStatus('Drag a recording region')
      const region = await tauriInvoke<Omit<RecordingRequest, 'settings'> | null>('open_region_selector')
      if (region) {
        await startRecordingWithRegion(region)
      } else {
        setStatus('Recording canceled')
      }
    } catch (error) {
      setIsRecording(false)
      setStatus(shortError(error))
    }
  }

  const startRecordingWithRegion = async (region: Omit<RecordingRequest, 'settings'>) => {
    const normalized = {
      x: Math.round(region.x),
      y: Math.round(region.y),
      width: makeEven(Math.max(8, region.width)),
      height: makeEven(Math.max(8, region.height)),
    }
    setRecordingRegion(normalized)
    const request: RecordingRequest = { ...normalized, settings }
    await tauriInvoke<string>('start_recording', { request })
    setIsRecording(true)
    setStatus(`Recording ${normalized.width}x${normalized.height} at ${normalized.x},${normalized.y} via ${settings.captureBackend === 'gdi' ? 'GDI' : 'Desktop Duplication'}`)
  }

  const selectCaptureBackend = (captureBackend: AppSettings['captureBackend']) => {
    const nextSettings = { ...settings, captureBackend }
    setSettings(nextSettings)
    void tauriInvoke('save_settings', { settings: nextSettings }).catch((error) => setStatus(shortError(error)))
  }

  const resetRecording = async () => {
    try {
      const request: RecordingRequest = { ...recordingRegion, settings }
      await tauriInvoke<string>('reset_recording', { request })
      setIsRecording(true)
      setStatus('Recording reset')
    } catch (error) {
      setStatus(shortError(error))
    }
  }

  const copyLastExport = async () => {
    if (!lastExport) {
      return
    }
    try {
      await tauriInvoke('copy_file_to_clipboard', { path: lastExport.path })
      setStatus(`Copied ${fileName(lastExport.path)}`)
    } catch (error) {
      setStatus(shortError(error))
    }
  }

  const openSaveFolder = async () => {
    try {
      await tauriInvoke('reveal_path', { path: settings.saveFolder })
    } catch (error) {
      setStatus(shortError(error))
    }
  }

  const checkForUpdates = async () => {
    if (!canUseTauri()) {
      setStatus('Updater only runs in the installed Tauri app')
      return
    }
    setIsCheckingUpdate(true)
    setStatus('Checking for updates')
    try {
      const update = await check()
      if (!update) {
        setStatus('Already up to date')
        return
      }
      setStatus(`Installing ${update.version}`)
      await notify('QuickClipper update', `Installing ${update.version}`)
      await update.downloadAndInstall()
      await relaunch()
    } catch (error) {
      setStatus(shortError(error))
    } finally {
      setIsCheckingUpdate(false)
    }
  }

  const seek = (value: number, exact = false) => {
    const next = normalizePlayableSourceTime(value, cuts, trimStart, trimEnd)
    currentTimeRef.current = next
    setCurrentTime(next)
    if (playbackRef.current) {
      playbackRef.current.currentTime = next
    }
    if (exact) {
      void showExactFrame(next)
    } else {
      clearExactFrame()
    }
  }

  const previewSourceTime = (value: number, exact = false) => {
    const next = clamp(value, 0, Math.max(clip?.durationSeconds ?? 0, 0))
    currentTimeRef.current = next
    setCurrentTime(next)
    if (playbackRef.current) {
      playbackRef.current.currentTime = next
    }
    if (exact) {
      void showExactFrame(next)
    } else {
      clearExactFrame()
    }
  }

  const snapTimelineTime = (value: number) => {
    if (!snappingEnabled || !clip) {
      return value
    }
    const threshold = Math.max(1 / Math.max(1, settings.frameRate), timelineViewport.span / 180)
    const candidates = [
      ...videoClipSegments.flatMap((segment) => [segment.start, segment.end]),
      ...audioClipSegments.flatMap((segment) => [segment.start, segment.end]),
    ]
    let best = value
    let bestDistance = threshold
    for (const candidate of candidates) {
      const distance = Math.abs(candidate - value)
      if (distance <= bestDistance) {
        best = candidate
        bestDistance = distance
      }
    }
    return best
  }

  const seekTimeline = (timelineSeconds: number, exact = false) => {
    seek(timelineToSourceTime(snapTimelineTime(timelineSeconds), videoClipSegments), exact)
  }
  const seekTimelineFromScrub = useEffectEvent((timelineSeconds: number) => {
    seekTimeline(timelineSeconds)
  })

  const stepFrame = (direction: -1 | 1, exact = true) => {
    if (!clip || videoClipSegments.length === 0) {
      return
    }
    setIsPlaying(false)
    const nextSource = adjacentSourceFrame(currentTimeRef.current, direction, settings.frameRate, videoClipSegments)
    previewSourceTime(nextSource, exact)
    if (!exact) {
      setIsTimelinePreviewing(true)
      exactFrameAfterStepRef.current = true
    }
  }

  const togglePlayback = () => {
    if (!clip) {
      return
    }
    if (isPlaying) {
      setCurrentTime(currentTimeRef.current)
      setIsPlaying(false)
      return
    }
    let start = currentTimeRef.current
    if (start >= trimEnd - 0.01) {
      start = normalizePlayableSourceTime(trimStart, cuts, trimStart, trimEnd)
      seek(start)
    } else {
      const normalized = normalizePlayableSourceTime(start, cuts, trimStart, trimEnd)
      if (Math.abs(normalized - start) > 0.001) {
        seek(normalized)
      }
    }
    clearExactFrame()
    setIsPlaying(true)
  }

  const toggleCut = () => {
    if (!clip) {
      return
    }
    if (pendingCutStart === null) {
      setPendingCutStart(currentTime)
      setStatus(`Cut start ${formatTime(currentTime)}`)
      return
    }
    const start = Math.min(pendingCutStart, currentTime)
    const end = Math.max(pendingCutStart, currentTime)
    setPendingCutStart(null)
    if (end - start < 0.033) {
      setStatus('Cut ignored')
      return
    }
    const targetTrack = selectedClip?.track ?? 'video'
    beginEdit('Cut')
    if (syncLinked) {
      setCuts((current) => mergeCuts([...current, { start, end }]))
      setAudioCuts((current) => mergeCuts([...current, { start, end }]))
    } else if (targetTrack === 'audio') {
      setAudioCuts((current) => mergeCuts([...current, { start, end }]))
    } else {
      setCuts((current) => mergeCuts([...current, { start, end }]))
    }
    clearClipSelection()
    finishEdit()
    setStatus(`Cut ${formatTime(start)} to ${formatTime(end)}`)
  }

  const removeCut = () => {
    const targetTrack = selectedClip?.track ?? 'video'
    const targetCuts = targetTrack === 'audio' ? audioCuts : cuts
    if (targetCuts.length === 0) {
      return
    }
    const nearest = targetCuts
      .map((cut, index) => ({ index, distance: Math.min(Math.abs(cut.start - currentTime), Math.abs(cut.end - currentTime)) }))
      .sort((a, b) => a.distance - b.distance)[0]
    beginEdit('Remove cut')
    if (syncLinked) {
      setCuts((current) => current.filter((_, index) => index !== nearest.index))
      setAudioCuts((current) => current.filter((_, index) => index !== nearest.index))
    } else if (targetTrack === 'audio') {
      setAudioCuts((current) => current.filter((_, index) => index !== nearest.index))
    } else {
      setCuts((current) => current.filter((_, index) => index !== nearest.index))
    }
    clearClipSelection()
    finishEdit()
  }

  const deleteSelectedClip = () => {
    const selections = selectedClips.length > 0 ? uniqueClipSelections(selectedClips) : selectedClip ? [selectedClip] : []
    if (selections.length === 0) {
      return
    }
    const videoRanges = selections
      .filter((selection) => selection.track === 'video')
      .map((selection) => videoClipSegments[selection.index])
      .filter(Boolean)
      .map((segment) => ({ start: segment.sourceStart, end: segment.sourceEnd }))
    const audioRanges = selections
      .filter((selection) => selection.track === 'audio')
      .map((selection) => audioClipSegments[selection.index])
      .filter(Boolean)
      .map((segment) => ({ start: segment.sourceStart, end: segment.sourceEnd }))
    if (videoRanges.length === 0 && audioRanges.length === 0) {
      return
    }
    beginEdit(selections.length > 1 ? 'Remove selected clips' : selections[0].track === 'audio' ? 'Remove audio clip' : 'Remove video clip')
    if (videoRanges.length > 0) {
      setCuts((current) => mergeCuts([...current, ...videoRanges]))
      if (syncLinked) {
        setAudioCuts((current) => mergeCuts([...current, ...videoRanges]))
      }
    }
    if (audioRanges.length > 0) {
      setAudioCuts((current) => mergeCuts([...current, ...audioRanges]))
      setSyncLinked(false)
    }
    clearClipSelection()
    finishEdit()
  }

  const applyQualityCap = () => {
    if (!clip) {
      return
    }
    const capSeconds = Math.max(1 / Math.max(1, settings.frameRate), Math.min(qualityMaxSeconds, clip.durationSeconds))
    const nextEnd = keptSeconds > capSeconds
      ? sourceEndForKeptDuration(videoClipSegments, capSeconds)
      : trimEnd
    const nextAudioEnd = audioKeptSeconds > capSeconds
      ? sourceEndForKeptDuration(audioClipSegments, capSeconds)
      : audioTrimEnd
    if (keptSeconds <= capSeconds && (!syncLinked || audioKeptSeconds <= capSeconds)) {
      setStatus('Already under max length')
      return
    }
    beginEdit('Length limit')
    if (keptSeconds > capSeconds) {
      setTrimEnd(nextEnd)
    }
    if (syncLinked) {
      setAudioTrimEnd(nextEnd)
    } else if (audioKeptSeconds > capSeconds) {
      setAudioTrimEnd(nextAudioEnd)
    }
    const nextTime = normalizePlayableSourceTime(Math.min(currentTimeRef.current, nextEnd), cuts, trimStart, nextEnd)
    currentTimeRef.current = nextTime
    setCurrentTime(nextTime)
    finishEdit()
    setStatus(`Max length applied at ${formatTime(capSeconds)}`)
    void showExactFrame(nextTime)
  }

  const resetClipEdits = () => {
    if (!clip) {
      return
    }
    beginEdit('Reset edits')
    setIsPlaying(false)
    currentTimeRef.current = 0
    setCurrentTime(0)
    clearExactFrame()
    setTrimStart(0)
    setTrimEnd(clip.durationSeconds)
    setTimelineOffset(0)
    setAudioTrimStart(0)
    setAudioTrimEnd(clip.durationSeconds)
    setAudioTimelineOffset(0)
    setCuts([])
    setAudioCuts([])
    setPendingCutStart(null)
    clearClipSelection()
    setCrop({ x: 0, y: 0, width: clip.width, height: clip.height })
    setOutputWidth(makeEven(clip.width))
    setOutputHeight(makeEven(clip.height))
    setAutoFit720(false)
    setTimelineZoom(1)
    setTimelineStart(0)
    setSyncLinked(true)
    setSettings((value) => ({ ...value, audioGainDb: 0 }))
    if (playbackRef.current) {
      playbackRef.current.currentTime = 0
      playbackRef.current.pause()
    }
    finishEdit()
    setStatus('Edits reset')
  }

  const applyCropChange = (nextCrop: Crop) => {
    setCrop(nextCrop)
    if (!autoFit720) {
      setOutputWidth(makeOutputDimension(nextCrop.width))
      setOutputHeight(makeOutputDimension(nextCrop.height))
    }
  }

  useEffect(() => {
    toggleRecordingRef.current = () => {
      void toggleRecording()
    }
    resetRecordingRef.current = () => {
      void resetRecording()
    }
    startRegionRecordingRef.current = (region) => {
      void startRecordingWithRegion(region)
    }
  })

  const undoEditFromKey = useEffectEvent(undoEdit)
  const redoEditFromKey = useEffectEvent(redoEdit)
  const deleteSelectedClipFromKey = useEffectEvent(deleteSelectedClip)
  const stepFrameFromKey = useEffectEvent(stepFrame)
  const togglePlaybackFromKey = useEffectEvent(togglePlayback)
  const toggleCutFromKey = useEffectEvent(toggleCut)

  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      const target = event.target as HTMLElement | null
      const isEditable = target?.tagName === 'INPUT' || target?.tagName === 'TEXTAREA' || target?.tagName === 'SELECT' || target?.isContentEditable
      if (event.key === 'Escape' && isViewerFullscreen) {
        event.preventDefault()
        exitViewerFullscreen()
        return
      }
      if (isEditable || settingsOpen) {
        return
      }
      if (!event.ctrlKey && !event.metaKey && !event.altKey && event.key.toLowerCase() === 'f' && !event.repeat) {
        event.preventDefault()
        toggleViewerFullscreen()
        return
      }
      const commandKey = event.ctrlKey || event.metaKey
      if (commandKey && (event.key === '+' || event.key === '=' || event.code === 'NumpadAdd')) {
        event.preventDefault()
        zoomViewerBy(1.25)
        return
      }
      if (commandKey && (event.key === '-' || event.code === 'NumpadSubtract')) {
        event.preventDefault()
        zoomViewerBy(1 / 1.25)
        return
      }
      if (commandKey && (event.key === '0' || event.code === 'Numpad0')) {
        event.preventDefault()
        resetViewer()
        return
      }
      if (commandKey && (event.key === '1' || event.code === 'Numpad1')) {
        event.preventDefault()
        showViewerActualSize()
        return
      }
      if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === 'z') {
        event.preventDefault()
        if (event.shiftKey) {
          redoEditFromKey()
        } else {
          undoEditFromKey()
        }
        return
      }
      if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === 'y') {
        event.preventDefault()
        redoEditFromKey()
        return
      }
      if (event.code === 'Space') {
        event.preventDefault()
        togglePlaybackFromKey()
        return
      }
      if (event.key === 'ArrowLeft') {
        event.preventDefault()
        stepFrameFromKey(-1, !event.repeat)
        return
      }
      if (event.key === 'ArrowRight') {
        event.preventDefault()
        stepFrameFromKey(1, !event.repeat)
        return
      }
      if (event.key === 'Delete' || event.key === 'Backspace') {
        event.preventDefault()
        deleteSelectedClipFromKey()
        return
      }
      if (event.key.toLowerCase() === 'x') {
        event.preventDefault()
        toggleCutFromKey()
      }
      if (event.key.toLowerCase() === 'c' && !event.repeat) {
        event.preventDefault()
        cScrubModeRef.current = true
        setIsCScrubbing(true)
      }
    }
    const onKeyUp = (event: KeyboardEvent) => {
      if (event.key === 'ArrowLeft' || event.key === 'ArrowRight') {
        if (exactFrameAfterStepRef.current) {
          exactFrameAfterStepRef.current = false
          setIsTimelinePreviewing(false)
          requestExactFrameFromKey(currentTimeRef.current)
        }
        return
      }
      if (event.key.toLowerCase() === 'c') {
        cScrubModeRef.current = false
        setIsCScrubbing(false)
      }
    }
    window.addEventListener('keydown', onKeyDown)
    window.addEventListener('keyup', onKeyUp)
    return () => {
      window.removeEventListener('keydown', onKeyDown)
      window.removeEventListener('keyup', onKeyUp)
    }
  }, [clip, exitViewerFullscreen, isViewerFullscreen, resetViewer, settingsOpen, showViewerActualSize, toggleViewerFullscreen, zoomViewerBy])

  const secondsFromTimelinePointer = (element: HTMLElement, clientX: number, bounded = true) => {
    const rect = element.getBoundingClientRect()
    const rawX = clientX - rect.left
    const x = bounded ? clamp(rawX, 0, rect.width) : rawX
    return timelineViewport.start + (x / Math.max(1, rect.width)) * timelineViewport.span
  }

  const setTimelineZoomAround = (nextZoom: number, anchorSeconds = sourceToTimelineTime(currentTime, videoClipSegments), anchorRatio = 0.5) => {
    const duration = timelineDurationFor(clip?.durationSeconds || 1, keptSeconds, timelineOffset, audioKeptSeconds, audioTimelineOffset)
    const zoom = clamp(nextZoom, 1, 80)
    const nextSpan = duration / zoom
    setTimelineZoom(zoom)
    setTimelineStart(clamp(anchorSeconds - nextSpan * anchorRatio, 0, Math.max(0, duration - nextSpan)))
  }

  const applyTimelineDrag = (element: HTMLElement, clientX: number) => {
    const secondsAt = secondsFromTimelinePointer(element, clientX, false)
    const drag = timelineDragRef.current
    if (!drag) {
      return
    }
    const sourceDuration = Math.max(clip?.durationSeconds ?? 0, 0)
    if (drag.mode === 'trim-start') {
      const trackEnd = drag.track === 'audio' ? audioTrimEnd : trimEnd
      const sourceAt = snapSourceTime(drag.sourceAnchor + (secondsAt - drag.timelineAnchor), drag.track)
      const nextStart = clamp(sourceAt, 0, Math.min(trackEnd - 0.033, sourceDuration))
      const nextOffset = Math.max(0, drag.timelineOffsetAnchor + nextStart - drag.sourceAnchor)
      if (syncLinked) {
        setTrimStart(nextStart)
        setAudioTrimStart(nextStart)
        setTimelineOffset(nextOffset)
        setAudioTimelineOffset(nextOffset)
      } else if (drag.track === 'audio') {
        setAudioTrimStart(nextStart)
        setAudioTimelineOffset(nextOffset)
      } else {
        setTrimStart(nextStart)
        setTimelineOffset(nextOffset)
      }
      previewSourceTime(nextStart)
      exactFrameAfterDragRef.current = true
      return
    }
    if (drag.mode === 'trim-end') {
      const trackStart = drag.track === 'audio' ? audioTrimStart : trimStart
      const sourceAt = snapSourceTime(drag.sourceAnchor + (secondsAt - drag.timelineAnchor), drag.track)
      const nextEnd = clamp(sourceAt, Math.max(trackStart + 0.033, 0), sourceDuration)
      if (syncLinked) {
        setTrimEnd(nextEnd)
        setAudioTrimEnd(nextEnd)
      } else if (drag.track === 'audio') {
        setAudioTrimEnd(nextEnd)
      } else {
        setTrimEnd(nextEnd)
      }
      previewSourceTime(nextEnd)
      exactFrameAfterDragRef.current = true
      return
    }
    if (drag.mode === 'cut-start' || drag.mode === 'cut-end') {
      const sourceAt = snapSourceTime(drag.sourceAnchor + (secondsAt - drag.timelineAnchor), drag.track)
      const activeCuts = drag.track === 'audio' ? audioCuts : cuts
      const trackStart = drag.track === 'audio' ? audioTrimStart : trimStart
      const trackEnd = drag.track === 'audio' ? audioTrimEnd : trimEnd
      const bounds = cutEditBounds(activeCuts, drag.cutIndex, drag.mode, trackStart, trackEnd)
      const previewAt = clamp(sourceAt, bounds.min, bounds.max)
      const updateCuts = (current: CutRange[]) => current.map((cut, index) => {
        if (index !== drag.cutIndex) {
          return cut
        }
        const nextBounds = cutEditBounds(current, drag.cutIndex, drag.mode, trackStart, trackEnd)
        const nextAt = clamp(sourceAt, nextBounds.min, nextBounds.max)
        return drag.mode === 'cut-start'
          ? { ...cut, start: nextAt }
          : { ...cut, end: nextAt }
      })
      if (syncLinked) {
        setCuts(updateCuts)
        setAudioCuts(updateCuts)
      } else if (drag.track === 'audio') {
        setAudioCuts(updateCuts)
      } else {
        setCuts(updateCuts)
      }
      previewSourceTime(previewAt)
      exactFrameAfterDragRef.current = true
      return
    }
    seekTimeline(secondsAt)
  }

  const snapSourceTime = (value: number, track: TimelineTrack) => {
    if (!snappingEnabled || !clip) {
      return value
    }
    const threshold = Math.max(1 / Math.max(1, settings.frameRate), timelineViewport.span / 180)
    const trackCuts = track === 'audio' ? audioCuts : cuts
    const otherCuts = track === 'audio' ? cuts : audioCuts
    const trackStart = track === 'audio' ? audioTrimStart : trimStart
    const trackEnd = track === 'audio' ? audioTrimEnd : trimEnd
    const otherStart = track === 'audio' ? trimStart : audioTrimStart
    const otherEnd = track === 'audio' ? trimEnd : audioTrimEnd
    const candidates = [
      currentTime,
      trackStart,
      trackEnd,
      otherStart,
      otherEnd,
      0,
      clip.durationSeconds,
      ...trackCuts.flatMap((cut) => [cut.start, cut.end]),
      ...otherCuts.flatMap((cut) => [cut.start, cut.end]),
    ]
    let best = value
    let bestDistance = threshold
    for (const candidate of candidates) {
      const distance = Math.abs(candidate - value)
      if (distance <= bestDistance) {
        best = candidate
        bestDistance = distance
      }
    }
    return best
  }

  useEffect(() => {
    const onPointerMove = (event: PointerEvent) => {
      const surface = timelineSurfaceRef.current
      if (!clip || !surface || !cScrubModeRef.current || settingsOpen) {
        return
      }
      const rect = surface.getBoundingClientRect()
      const x = clamp(event.clientX - rect.left, 0, rect.width)
      const timelineSeconds = timelineViewport.start + (x / Math.max(1, rect.width)) * timelineViewport.span
      seekTimelineFromScrub(timelineSeconds)
    }
    const onBlur = () => {
      cScrubModeRef.current = false
      setIsCScrubbing(false)
    }
    window.addEventListener('pointermove', onPointerMove)
    window.addEventListener('blur', onBlur)
    return () => {
      window.removeEventListener('pointermove', onPointerMove)
      window.removeEventListener('blur', onBlur)
    }
  }, [clip, settingsOpen, timelineViewport])

  useEffect(() => {
    const onPointerDown = (event: PointerEvent) => {
      const target = event.target as HTMLElement | null
      if (!target?.closest('.timeline-clip')) {
        clearClipSelection()
      }
    }
    window.addEventListener('pointerdown', onPointerDown)
    return () => window.removeEventListener('pointerdown', onPointerDown)
  }, [])

  useEffect(() => {
    if (!canUseTauri()) {
      return
    }

    let disposed = false
    unregisterAll()
      .then(async () => {
        if (disposed) {
          return
        }
        await register(settings.recordHotkey, (event) => {
          if (event.state === 'Pressed') {
            toggleRecordingRef.current()
          }
        })
        await register(settings.resetHotkey, (event) => {
          if (event.state === 'Pressed') {
            resetRecordingRef.current()
          }
        })
      })
      .catch((error) => setStatus(`Hotkey unavailable: ${shortError(error)}`))

    return () => {
      disposed = true
      void unregisterAll()
    }
  }, [settings.recordHotkey, settings.resetHotkey])

  useEffect(() => {
    if (!canUseTauri()) {
      return
    }
    const cleanups = Promise.all([
      listen('tray-record-toggle', () => toggleRecordingRef.current()),
      listen('tray-record-reset', () => resetRecordingRef.current()),
      listen<Omit<RecordingRequest, 'settings'>>('region-selected', (event) => {
        startRegionRecordingRef.current(event.payload)
      }),
    ])
    return () => {
      void cleanups.then((items) => items.forEach((cleanup) => cleanup()))
    }
  }, [])

  const getSegmentFrames = (segment: DisplayClip) => {
    if (!preview || preview.frames.length === 0) {
      return []
    }
    const startIndex = clamp(Math.floor(segment.sourceStart * preview.fps), 0, preview.frames.length - 1)
    const endIndex = clamp(Math.ceil(segment.sourceEnd * preview.fps), startIndex + 1, preview.frames.length)
    const frames = preview.frames.slice(startIndex, endIndex)
    const visibleShare = (segment.end - segment.start) / Math.max(0.001, timelineViewport.span)
    const desired = clamp(Math.ceil(visibleShare * 42), 1, 36)
    const step = Math.max(1, Math.floor(frames.length / desired))
    return frames.filter((_, index) => index % step === 0).slice(0, desired)
  }

  const selectClipsInMarquee = (element: HTMLElement, selection: NonNullable<MarqueeSelection>) => {
    const rect = element.getBoundingClientRect()
    const left = Math.min(selection.startX, selection.currentX)
    const right = Math.max(selection.startX, selection.currentX)
    const top = Math.min(selection.startY, selection.currentY)
    const bottom = Math.max(selection.startY, selection.currentY)
    const selections: ClipSelection[] = []

    const collect = (track: TimelineTrack, segments: DisplayClip[]) => {
      const trackElement = element.querySelector<HTMLElement>(track === 'video' ? '.video-track' : '.audio-track')
      if (!trackElement) {
        return
      }
      const trackRect = trackElement.getBoundingClientRect()
      const trackTop = trackRect.top - rect.top
      const trackBottom = trackRect.bottom - rect.top
      if (bottom < trackTop || top > trackBottom) {
        return
      }
      segments.forEach((segment, index) => {
        const segmentLeft = ((segment.start - timelineViewport.start) / Math.max(0.001, timelineViewport.span)) * rect.width
        const segmentRight = ((segment.end - timelineViewport.start) / Math.max(0.001, timelineViewport.span)) * rect.width
        if (segmentRight >= left && segmentLeft <= right) {
          selections.push({ track, index })
        }
      })
    }

    collect('video', videoClipSegments)
    collect('audio', audioClipSegments)
    return uniqueClipSelections(selections)
  }

  const updateMarqueeSelection = (drag: Extract<TimelineDragState, { mode: 'marquee' }>, clientX: number, clientY: number) => {
    const rect = drag.element.getBoundingClientRect()
    const nextSelection = {
      startX: drag.startX,
      startY: drag.startY,
      currentX: clamp(clientX - rect.left, 0, rect.width),
      currentY: clamp(clientY - rect.top, 0, rect.height),
    }
    timelineDragRef.current = { ...drag, currentX: nextSelection.currentX, currentY: nextSelection.currentY }
    setMarqueeSelection(nextSelection)
    const nextClips = selectClipsInMarquee(drag.element, nextSelection)
    setSelectedClips(nextClips)
    setSelectedClip(nextClips[0] ?? null)
  }

  const beginTimelineDrag = (event: ReactPointerEvent<HTMLElement>, drag: TimelineDragInput) => {
    const element = timelineSurfaceRef.current
    if (!element) {
      return
    }
    event.preventDefault()
    event.stopPropagation()
    if (drag.mode !== 'seek' && drag.mode !== 'pan') {
      beginEdit('Timeline drag')
      setIsTimelinePreviewing(true)
    }
    if (drag.mode === 'pan' || drag.mode === 'seek') {
      timelineDragRef.current = { ...drag, element } as TimelineDragState
    } else {
      timelineDragRef.current = {
        ...drag,
        element,
        timelineAnchor: secondsFromTimelinePointer(element, event.clientX, false),
        timelineOffsetAnchor: drag.mode === 'trim-start' || drag.mode === 'trim-end'
          ? (drag.track === 'audio' ? audioTimelineOffset : timelineOffset)
          : timelineOffset,
      } as TimelineDragState
    }
    event.currentTarget.setPointerCapture(event.pointerId)
  }

  const edgeDragForSegment = (segment: DisplayClip, edge: 'left' | 'right', track: TimelineTrack): TimelineDragInput => {
    const trackCuts = track === 'audio' ? audioCuts : cuts
    if (edge === 'left') {
      const cutIndex = trackCuts.findIndex((cut) => Math.abs(cut.end - segment.sourceStart) < 0.01)
      return cutIndex >= 0
        ? { mode: 'cut-end', cutIndex, track, sourceAnchor: segment.sourceStart }
        : { mode: 'trim-start', track, sourceAnchor: segment.sourceStart }
    }
    const cutIndex = trackCuts.findIndex((cut) => Math.abs(cut.start - segment.sourceEnd) < 0.01)
    return cutIndex >= 0
      ? { mode: 'cut-start', cutIndex, track, sourceAnchor: segment.sourceEnd }
      : { mode: 'trim-end', track, sourceAnchor: segment.sourceEnd }
  }

  const isClipSelected = (track: TimelineTrack, index: number) => {
    if (selectedClips.some((selection) => selection.track === track && selection.index === index)) {
      return true
    }
    if (syncLinked && selectedClips.some((selection) => selection.index === index)) {
      return true
    }
    if (!selectedClip || selectedClip.index !== index) {
      return false
    }
    return selectedClip.track === track || syncLinked
  }

  const toggleTrackLink = () => {
    const nextLinked = !syncLinked
    if (nextLinked && clip) {
      beginEdit('Link tracks')
      setAudioTrimStart(trimStart)
      setAudioTrimEnd(trimEnd)
      setAudioTimelineOffset(timelineOffset)
      setAudioCuts(cloneCuts(cuts))
      finishEdit()
    }
    setSyncLinked(nextLinked)
  }

  return (
    <main className="editor-shell">
      <header className="topbar">
        <div className="brand">
          <Aperture className="size-5 text-[#5fb1ff]" />
          <div>
            <strong>QuickClipper</strong>
            <span>{clipName}</span>
          </div>
        </div>
        <div className="top-center">
          <span>{clip ? `${formatTime(keptSeconds)} kept` : 'Open or record a clip'}</span>
        </div>
        <div className="top-actions">
          <Button size="icon" variant="subtle" disabled={!canUndo} onClick={undoEdit}>
            <Undo2 />
          </Button>
          <Button size="icon" variant="subtle" disabled={!canRedo} onClick={redoEdit}>
            <Redo2 />
          </Button>
          <label className="top-toggle">
            <input
              type="checkbox"
              checked={settings.includeVideo}
              onChange={(event) => setSettings((value) => ({
                ...value,
                includeVideo: event.target.checked,
                includeAudio: event.target.checked ? value.includeAudio : true,
              }))}
            />
            Video
          </label>
          <label className="top-toggle">
            <input
              type="checkbox"
              checked={settings.includeAudio}
              onChange={(event) => setSettings((value) => ({
                ...value,
                includeAudio: event.target.checked,
                includeVideo: event.target.checked ? value.includeVideo : true,
              }))}
            />
            Audio
          </label>
          {settings.includeAudio && (
            <select
              className="top-select has-tooltip"
              data-tooltip="Audio Quality: Best uses 24-bit PCM for WAV and 320 kbps AAC for MP4; Optimized uses 16-bit PCM or 320 kbps AAC; Standard uses 16-bit 44.1 kHz PCM or 160 kbps AAC."
              title="Audio quality preset"
              value={settings.audioQuality ?? (settings.bestAudio ? 'best' : 'standard')}
              onChange={(event) => setSettings((value) => ({
                ...value,
                audioQuality: event.target.value as 'optimized' | 'best' | 'standard',
                bestAudio: event.target.value === 'best',
              }))}
            >
              <option value="optimized">Optimized (320k)</option>
              <option value="best">Best Audio</option>
              <option value="standard">Standard (160k)</option>
            </select>
          )}
          <label
            className="top-toggle has-tooltip"
            data-tooltip={`Size Cap targets ${settings.maxMegabytes.toFixed(1)} MB by adjusting export bitrate and retrying up to 5 times. Audio-only WAV uses the highest PCM profile that fits. Turn it off to preserve source quality.`}
            title="Target a maximum exported file size."
          >
            <input
              type="checkbox"
              checked={settings.sizeCapEnabled}
              onChange={(event) => setSettings((value) => ({ ...value, sizeCapEnabled: event.target.checked }))}
            />
            Size Cap
          </label>
          {isExporting ? (
            <Button variant="danger" disabled={isStoppingExport} onClick={stopExport}>
              <Square />
              {isStoppingExport ? 'Stopping…' : 'Stop Export'}
            </Button>
          ) : (
            <Button variant="primary" disabled={!clip} onClick={exportClip}>
              <Upload />
              {settings.includeVideo ? 'Export' : 'Export WAV'}
            </Button>
          )}
          <Button size="icon" variant="subtle" disabled={!lastExport} onClick={copyLastExport} title="Copy exported file to clipboard">
            <Clipboard />
          </Button>
          <Button size="icon" variant="subtle" onClick={openSaveFolder} title="Open export folder">
            <FolderOpen />
          </Button>
          <Button size="icon" variant="subtle" onClick={() => setSettingsOpen(true)}>
            <Settings2 />
          </Button>
        </div>
      </header>

      <section className="workspace">
        <aside className="media-column">
          <Card className="media-bin">
            <CardHeader className="flex-row items-center justify-between">
              <CardTitle>Media Pool</CardTitle>
              <Badge>{clip ? '1 clip' : 'empty'}</Badge>
            </CardHeader>
            <CardContent className="space-y-3">
              <Button className="w-full justify-start" variant="primary" onClick={openVideo}>
                <FolderOpen />
                Open Video
              </Button>
              <Button className="w-full justify-start" variant={isRecording ? 'danger' : 'default'} onClick={toggleRecording}>
                {isRecording ? <Square /> : <Radio />}
                {isRecording ? 'Stop Recording' : 'Record Region'}
              </Button>
              <Button className="w-full justify-start" variant="subtle" disabled={!isRecording} onClick={resetRecording}>
                <Timer />
                Reset Recording
              </Button>
              <Button className="w-full justify-start" variant="subtle" disabled={!clip} onClick={resetClipEdits}>
                <RotateCcw />
                Reset Edits
              </Button>
              <div className="media-item selected">
                <Film className="size-8" />
                <div>
                  <strong>{clipName}</strong>
                  <span>{clip ? `${clip.width}x${clip.height} | ${formatTime(clip.durationSeconds)}` : 'No active source'}</span>
                </div>
              </div>
              {lastExport && (
                <div className="grid grid-cols-2 gap-2">
                  <Button className="justify-start" variant="subtle" onClick={copyLastExport}>
                    <Copy />
                    Copy File
                  </Button>
                  <Button className="justify-start" variant="subtle" onClick={() => void tauriInvoke('reveal_path', { path: lastExport.path })}>
                    <Upload />
                    Reveal
                  </Button>
                </div>
              )}
            </CardContent>
          </Card>

          <Card>
            <CardHeader>
              <CardTitle>Capture Region</CardTitle>
            </CardHeader>
            <CardContent className="space-y-2">
              <div className="space-y-1">
                <Label>Capture Method</Label>
                <Select value={settings.captureBackend} disabled={isRecording} onChange={(event) => selectCaptureBackend(event.target.value as AppSettings['captureBackend'])}>
                  <option value="desktopDuplication">Desktop Duplication (GPU)</option>
                  <option value="gdi">GDI (legacy)</option>
                </Select>
              </div>
              <div className="grid grid-cols-2 gap-2">
                <NumberField label="X" value={recordingRegion.x} onChange={(x) => setRecordingRegion((region) => ({ ...region, x }))} />
                <NumberField label="Y" value={recordingRegion.y} onChange={(y) => setRecordingRegion((region) => ({ ...region, y }))} />
                <NumberField label="W" value={recordingRegion.width} onChange={(width) => setRecordingRegion((region) => ({ ...region, width }))} />
                <NumberField label="H" value={recordingRegion.height} onChange={(height) => setRecordingRegion((region) => ({ ...region, height }))} />
              </div>
            </CardContent>
          </Card>
        </aside>

        <section className={`viewer-column${isViewerFullscreen ? ' viewer-fullscreen' : ''}`}>
          <div className="viewer-toolbar">
            <span className="viewer-title">Viewer</span>
            <div className="viewer-toolbar-actions">
              <Button size="sm" variant="subtle" disabled={!clip || isCopyingFrame} onClick={copyVideoFrame} title="Copy the current video frame to the clipboard at full source resolution, without crop or resize">
                <Camera />
                {isCopyingFrame ? 'Copying…' : 'Copy Frame'}
              </Button>
              <div className="viewer-zoom-controls" aria-label="Viewer zoom controls">
                <Button size="icon" variant="ghost" disabled={!viewerSourceSize} aria-label="Zoom out" title="Zoom Out (Ctrl+-)" onClick={() => zoomViewerBy(1 / 1.25)}>
                  <Minus />
                </Button>
                <span className="viewer-zoom-readout" aria-live="polite">{Math.round(viewerView.scale * 100)}%</span>
                <Button size="icon" variant="ghost" disabled={!viewerSourceSize} aria-label="Zoom in" title="Zoom In (Ctrl++)" onClick={() => zoomViewerBy(1.25)}>
                  <Plus />
                </Button>
                <Button size="sm" variant={viewerView.mode === 'fit' ? 'primary' : 'ghost'} disabled={!viewerSourceSize} title="Fit / Reset View (Ctrl+0)" onClick={resetViewer}>
                  Fit
                </Button>
                <Button size="sm" variant={viewerView.mode === 'actual' ? 'primary' : 'ghost'} disabled={!viewerSourceSize} title="Actual Size (Ctrl+1)" onClick={showViewerActualSize}>
                  1:1
                </Button>
                <Button size="icon" variant={isViewerFullscreen ? 'primary' : 'ghost'} disabled={!viewerSourceSize} aria-label={isViewerFullscreen ? 'Exit fullscreen viewer' : 'Fullscreen viewer'} title={`${isViewerFullscreen ? 'Exit Fullscreen' : 'Fullscreen Viewer'} (F)`} onClick={toggleViewerFullscreen}>
                  {isViewerFullscreen ? <Minimize2 /> : <Maximize2 />}
                </Button>
              </div>
              <Badge>{clip ? `${formatTime(currentTime)} / ${formatTime(clip.durationSeconds)}` : '00:00.000'}</Badge>
            </div>
          </div>
          <div
            className={`viewer${isViewerPanning ? ' panning' : ''}`}
            ref={viewerSurfaceRef}
            onPointerDown={beginViewerPan}
            onPointerMove={moveViewerPan}
            onPointerUp={endViewerPan}
            onPointerCancel={endViewerPan}
            onLostPointerCapture={endViewerPan}
          >
            {clipMediaSrc && (
              <video
                className="viewer-video"
                ref={playbackRef}
                src={clipMediaSrc}
                preload="auto"
                playsInline
                onLoadedMetadata={(event) => {
                  const media = event.currentTarget
                  if (media.videoWidth > 0 && media.videoHeight > 0) {
                    setViewerMediaSize({ width: media.videoWidth, height: media.videoHeight })
                  }
                  bumpViewerDraw()
                }}
                onSeeked={bumpViewerDraw}
              />
            )}
            {previewSrc && (
              <img
                className="viewer-still"
                ref={viewerStillRef}
                src={previewSrc}
                alt=""
                onLoad={(event) => {
                  const image = event.currentTarget
                  const video = playbackRef.current
                  const videoHasSize = Boolean(video && video.videoWidth > 0 && video.videoHeight > 0)
                  if (image.naturalWidth > 0 && image.naturalHeight > 0 && !videoHasSize) {
                    setViewerMediaSize({ width: image.naturalWidth, height: image.naturalHeight })
                  }
                  bumpViewerDraw()
                }}
              />
            )}
            <canvas className="viewer-canvas" ref={viewerCanvasRef} />
            {!previewSrc && !clipMediaSrc && (
              <div className="empty-viewer">
                <Film className="size-12" />
                <span>{isPreparing ? 'Preparing preview' : 'Open or record a clip'}</span>
              </div>
            )}
            {clip && <CropOverlay crop={crop} sourceWidth={clip.width} sourceHeight={clip.height} view={viewerView} forceAspectLock={autoFit720} onBeginEdit={() => beginEdit('Crop')} onEndEdit={finishEdit} onChange={applyCropChange} />}
          </div>
          <div className="transport">
            <Button size="icon" variant="ghost" onClick={() => stepFrame(-1, true)}>
              <SkipBack />
            </Button>
            <Button size="icon" variant="primary" disabled={!clip} onClick={togglePlayback}>
              {isPlaying ? <Pause /> : <Play />}
            </Button>
            <Button size="icon" variant="ghost" onClick={() => stepFrame(1, true)}>
              <SkipForward />
            </Button>
            <Button variant="subtle" disabled={!clip} onClick={toggleCut}>
              <Scissors />
              {pendingCutStart === null ? 'Start Cut' : 'End Cut'}
            </Button>
            <Button variant="subtle" disabled={!clip || (selectedClip?.track === 'audio' ? audioCuts.length === 0 : cuts.length === 0)} onClick={removeCut}>
              Remove Cut
            </Button>
            <Button variant="subtle" disabled={!selectedClip && selectedClips.length === 0} onClick={deleteSelectedClip}>
              <Trash2 />
              Delete Clip
            </Button>
          </div>
        </section>

        <aside className="inspector">
          <Card>
            <CardHeader>
              <CardTitle>Inspector</CardTitle>
            </CardHeader>
            <CardContent className="space-y-4">
              <div className="section-label">
                <FolderOpen className="size-4" />
                Output
              </div>
              <div className="grid grid-cols-2 gap-2">
                <NumberField label="FPS" value={settings.frameRate} onChange={(frameRate) => setSettings((value) => ({ ...value, frameRate: clamp(frameRate, 5, 60) }))} />
                <NumberField label="Quality" value={settings.qualityTargetKbps} step={500} suffix="kbps" onChange={(qualityTargetKbps) => setSettings((value) => ({ ...value, qualityTargetKbps: clamp(qualityTargetKbps, 500, 50000) }))} />
              </div>
              <NumberField label="Max MB" value={settings.maxMegabytes} step={0.1} onChange={(maxMegabytes) => setSettings((value) => ({ ...value, maxMegabytes }))} />
              <Button className="w-full" variant="subtle" disabled={!clip} onClick={applyQualityCap}>
                <Timer />
                Apply Max Length
              </Button>
              <Separator />
              <div className="section-label">
                <CropIcon className="size-4" />
                Crop
              </div>
              <div className="grid grid-cols-2 gap-2">
                <NumberField label="X" value={crop.x} onChange={(x) => { beginEdit('Crop field'); applyCropChange(clip ? clampCrop({ ...crop, x }, clip.width, clip.height) : { ...crop, x }); finishEdit() }} />
                <NumberField label="Y" value={crop.y} onChange={(y) => { beginEdit('Crop field'); applyCropChange(clip ? clampCrop({ ...crop, y }, clip.width, clip.height) : { ...crop, y }); finishEdit() }} />
                <NumberField label="W" value={crop.width} onChange={(width) => { beginEdit('Crop field'); applyCropChange(clip ? clampCrop({ ...crop, width: makeEven(width) }, clip.width, clip.height) : { ...crop, width: makeEven(width) }); finishEdit() }} />
                <NumberField label="H" value={crop.height} onChange={(height) => { beginEdit('Crop field'); applyCropChange(clip ? clampCrop({ ...crop, height: makeEven(height) }, clip.width, clip.height) : { ...crop, height: makeEven(height) }); finishEdit() }} />
              </div>
              <Separator />
              <div className="section-label">
                <Maximize2 className="size-4" />
                Resize
              </div>
              <div className="grid grid-cols-2 gap-2">
                <NumberField label="W" value={outputWidth} onChange={(width) => { beginEdit('Resize field'); setAutoFit720(false); setOutputWidth(makeOutputDimension(width)); finishEdit() }} />
                <NumberField label="H" value={outputHeight} onChange={(height) => { beginEdit('Resize field'); setAutoFit720(false); setOutputHeight(makeOutputDimension(height)); finishEdit() }} />
              </div>
              <div className="output-size-summary">Crop {crop.width}x{crop.height} → Output {outputWidth}x{outputHeight}</div>
              {Math.abs(crop.width / Math.max(1, crop.height) - outputWidth / Math.max(1, outputHeight)) > 0.001 && (
                <div className="output-size-warning">Aspect mismatch: export will stretch</div>
              )}
              <Button
                className="w-full"
                variant={autoFit720 ? 'primary' : 'default'}
                onClick={() => {
                  if (!clip) {
                    return
                  }
                  beginEdit('Auto resize')
                  setAutoFit720(true)
                  setCrop(cropForAspect(clip.width, clip.height, 16 / 9))
                  setOutputWidth(1280)
                  setOutputHeight(720)
                  finishEdit()
                }}
                disabled={!clip}
              >
                Auto 1280 x 720
              </Button>
              <Button className="w-full" variant="subtle" onClick={saveSettings}>
                <Settings2 />
                Save Settings
              </Button>
              <Button className="w-full" variant="subtle" disabled={!clip || isExporting || !settings.includeVideo} onClick={benchmarkEncoders}>
                <Gauge />
                Benchmark Encoders
              </Button>
              {benchmarkResults.length > 0 && (
                <div className="benchmark-list">
                  {benchmarkResults.map((result) => (
                    <div key={result.encoderKey} className={result.success ? 'benchmark-row' : 'benchmark-row failed'}>
                      <strong>{result.encoderLabel}</strong>
                      <span>{result.success ? `${result.seconds.toFixed(1)}s | ${formatBytes(result.bytes)}` : 'unavailable'}</span>
                    </div>
                  ))}
                </div>
              )}
            </CardContent>
          </Card>
        </aside>
      </section>

      <section className="timeline-panel">
        <div className="timeline-header">
          <div className="timecode">{formatTime(currentTime)}</div>
          <div className="zoom-control">
            <Button size="icon" variant={snappingEnabled ? 'primary' : 'subtle'} onClick={() => setSnappingEnabled((value) => !value)}>
              <Magnet />
            </Button>
            <Button size="icon" variant={syncLinked ? 'primary' : 'subtle'} onClick={toggleTrackLink}>
              <Link2 />
            </Button>
            <span>Zoom</span>
            <input
              aria-label="Timeline zoom"
              className="timeline-zoom"
              type="range"
              min={1}
              max={80}
              step={0.1}
              value={timelineZoom}
              disabled={!clip}
              onChange={(event) => setTimelineZoomAround(Number(event.target.value))}
            />
            <Badge>{timelineZoom.toFixed(1)}x</Badge>
          </div>
          <div className="timeline-stats">
            <Badge>{formatTime(trimStart)} to {formatTime(trimEnd)}</Badge>
            <Badge>{formatTime(keptSeconds)} kept</Badge>
            <Badge>{settings.sizeCapEnabled ? `${settings.maxMegabytes.toFixed(1)} MB target` : 'source quality'}</Badge>
            <Badge>{formatTime(qualityMaxSeconds)} max length</Badge>
          </div>
        </div>
        <div className="timeline-grid">
          <div className="track-labels">
            <div className="track-label-spacer" />
            <TrackLabel name="V1" detail="Video 1" />
            <TrackLabel name="A1" detail={clip ? 'Source' : ''} />
          </div>
          <div
            className={isTimelinePanning ? 'tracks panning' : isCScrubbing ? 'tracks c-scrub' : 'tracks'}
            ref={timelineSurfaceRef}
            onPointerDown={(event) => {
              if (!clip) {
                return
              }
              if (event.button === 1) {
                event.preventDefault()
                setIsTimelinePanning(true)
                timelineDragRef.current = { mode: 'pan', element: event.currentTarget, startX: event.clientX, startTimelineStart: timelineStart }
                event.currentTarget.setPointerCapture(event.pointerId)
                return
              }
              if (event.button === 0 && !cScrubModeRef.current) {
                const target = event.target as HTMLElement | null
                if (target?.closest('.timeline-clip') || target?.closest('.ruler')) {
                  return
                }
                const rect = event.currentTarget.getBoundingClientRect()
                const startX = clamp(event.clientX - rect.left, 0, rect.width)
                const startY = clamp(event.clientY - rect.top, 0, rect.height)
                const selection = { startX, startY, currentX: startX, currentY: startY }
                clearClipSelection()
                setMarqueeSelection(selection)
                timelineDragRef.current = { mode: 'marquee', element: event.currentTarget, ...selection }
                event.currentTarget.setPointerCapture(event.pointerId)
                return
              }
              if (event.button === 0 && cScrubModeRef.current) {
                timelineDragRef.current = { mode: 'seek', element: event.currentTarget }
                event.currentTarget.setPointerCapture(event.pointerId)
                applyTimelineDrag(event.currentTarget, event.clientX)
              }
            }}
            onPointerMove={(event) => {
              const drag = timelineDragRef.current
              if (!clip || !drag) {
                return
              }
              if (drag.mode === 'marquee') {
                updateMarqueeSelection(drag, event.clientX, event.clientY)
                return
              }
              if (drag.mode === 'pan') {
                const deltaSeconds = ((drag.startX - event.clientX) / Math.max(1, drag.element.getBoundingClientRect().width)) * timelineViewport.span
                setTimelineStart(clamp(drag.startTimelineStart + deltaSeconds, 0, Math.max(0, timelineViewport.duration - timelineViewport.span)))
                return
              }
              applyTimelineDrag(drag.element, event.clientX)
            }}
            onPointerUp={(event) => {
              const drag = timelineDragRef.current
              if (drag?.mode === 'marquee') {
                updateMarqueeSelection(drag, event.clientX, event.clientY)
              }
              timelineDragRef.current = null
              setMarqueeSelection(null)
              setIsTimelinePanning(false)
              setIsTimelinePreviewing(false)
              finishEdit()
              if (exactFrameAfterDragRef.current) {
                exactFrameAfterDragRef.current = false
                void showExactFrame(currentTimeRef.current)
              }
              if (event.currentTarget.hasPointerCapture(event.pointerId)) {
                event.currentTarget.releasePointerCapture(event.pointerId)
              }
            }}
            onPointerCancel={(event) => {
              timelineDragRef.current = null
              setMarqueeSelection(null)
              setIsTimelinePanning(false)
              setIsTimelinePreviewing(false)
              exactFrameAfterDragRef.current = false
              finishEdit()
              if (event.currentTarget.hasPointerCapture(event.pointerId)) {
                event.currentTarget.releasePointerCapture(event.pointerId)
              }
            }}
            onAuxClick={(event) => {
              if (event.button === 1) {
                event.preventDefault()
              }
            }}
            onWheel={(event) => {
              if (!clip) {
                return
              }
              event.preventDefault()
              const rect = event.currentTarget.getBoundingClientRect()
              const ratio = clamp((event.clientX - rect.left) / Math.max(1, rect.width), 0, 1)
              const anchor = timelineViewport.start + ratio * timelineViewport.span
              if (event.ctrlKey || event.altKey) {
                setTimelineZoomAround(timelineZoom * (event.deltaY < 0 ? 1.18 : 0.85), anchor, ratio)
              } else {
                const deltaSeconds = (event.deltaY / Math.max(1, rect.width)) * timelineViewport.span
                setTimelineStart((value) => clamp(value + deltaSeconds, 0, Math.max(0, timelineViewport.duration - timelineViewport.span)))
              }
            }}
          >
            <div
              className="ruler"
              onPointerDown={(event) => {
                if (!clip || event.button !== 0) {
                  return
                }
                beginTimelineDrag(event, { mode: 'seek' })
                seekTimeline(secondsFromTimelinePointer(event.currentTarget, event.clientX))
              }}
            >
              {Array.from({ length: 12 }, (_, index) => (
                <span key={index}>{formatTime(timelineViewport.start + timelineViewport.span * (index / 11))}</span>
              ))}
            </div>
            <div className="playhead" style={{ left: `${clip ? timelinePercent(sourceToTimelineTime(currentTime, videoClipSegments), timelineViewport) : 0}%` }} />
            <div className="video-track">
              {videoClipSegments.map((segment, index) => (
                <div
                  className={isClipSelected('video', index) ? 'video-clip timeline-clip selected' : 'video-clip timeline-clip'}
                  key={`video-${segment.start}-${segment.end}`}
                  onPointerDown={(event) => {
                    if (event.button !== 0) {
                      return
                    }
                    if (cScrubModeRef.current && timelineSurfaceRef.current) {
                      seekTimeline(secondsFromTimelinePointer(timelineSurfaceRef.current, event.clientX))
                      return
                    }
                    event.stopPropagation()
                    selectClip({ track: 'video', index })
                  }}
                  style={timelineRangeStyle(segment.start, segment.end, timelineViewport)}
                >
                  <span className="clip-edge clip-edge-left" onPointerDown={(event) => beginTimelineDrag(event, edgeDragForSegment(segment, 'left', 'video'))} />
                  <span className="clip-edge clip-edge-right" onPointerDown={(event) => beginTimelineDrag(event, edgeDragForSegment(segment, 'right', 'video'))} />
                  {getSegmentFrames(segment).map((frame) => (
                    <img key={frame} src={convertFileSrc(frame)} alt="" />
                  ))}
                  <strong>{index === 0 ? clipName : `${clipName} (${index + 1})`}</strong>
                </div>
              ))}
            </div>
            <div className={clip ? 'audio-track has-source' : 'audio-track'}>
              {audioClipSegments.map((segment, index) => (
                <div
                  className={isClipSelected('audio', index) ? 'audio-clip timeline-clip selected' : 'audio-clip timeline-clip'}
                  key={`audio-${segment.start}-${segment.end}`}
                  onPointerDown={(event) => {
                    if (event.button !== 0) {
                      return
                    }
                    if (cScrubModeRef.current && timelineSurfaceRef.current) {
                      seekTimeline(secondsFromTimelinePointer(timelineSurfaceRef.current, event.clientX))
                      return
                    }
                    event.stopPropagation()
                    selectClip({ track: 'audio', index })
                  }}
                  style={timelineRangeStyle(segment.start, segment.end, timelineViewport)}
                >
                  <span className="clip-edge clip-edge-left" onPointerDown={(event) => beginTimelineDrag(event, edgeDragForSegment(segment, 'left', 'audio'))} />
                  <span className="clip-edge clip-edge-right" onPointerDown={(event) => beginTimelineDrag(event, edgeDragForSegment(segment, 'right', 'audio'))} />
                  <TimelineWaveform
                    gainDb={settings.audioGainDb}
                    isLoading={isGeneratingWaveform}
                    onBeginEdit={() => beginEdit('Audio gain')}
                    onEndEdit={finishEdit}
                    onGainChange={(audioGainDb) => setSettings((value) => ({ ...value, audioGainDb }))}
                    range={{ start: segment.sourceStart, end: segment.sourceEnd }}
                    waveform={waveform}
                  />
                </div>
              ))}
            </div>
            {pendingCutStart !== null && (
              <div
                className="pending-cut-band"
                style={timelineRangeStyle(
                  sourceToTimelineTime(Math.min(pendingCutStart, currentTime), videoClipSegments),
                  sourceToTimelineTime(Math.max(pendingCutStart, currentTime), videoClipSegments),
                  timelineViewport,
                )}
              />
            )}
            {marqueeSelection && (
              <div className="selection-marquee" style={marqueeSelectionStyle(marqueeSelection)} />
            )}
          </div>
        </div>
        <div className="trim-controls">
          <Button variant="subtle" disabled={!clip} onClick={() => {
            beginEdit('Set in')
            if (syncLinked || selectedClip?.track !== 'audio') {
              setTrimStart(currentTime)
            }
            if (syncLinked || selectedClip?.track === 'audio') {
              setAudioTrimStart(currentTime)
            }
            finishEdit()
          }}>
            <ChevronLeft />
            Set In
          </Button>
          <Button variant="subtle" disabled={!clip} onClick={() => {
            beginEdit('Set out')
            if (syncLinked || selectedClip?.track !== 'audio') {
              setTrimEnd(currentTime)
            }
            if (syncLinked || selectedClip?.track === 'audio') {
              setAudioTrimEnd(currentTime)
            }
            finishEdit()
          }}>
            Set Out
            <ChevronRight />
          </Button>
          <Button variant="subtle" disabled={!clip} onClick={() => seek(selectedClip?.track === 'audio' ? audioTrimStart : trimStart, true)}>
            Go In
          </Button>
          <Button variant="subtle" disabled={!clip} onClick={() => seek(selectedClip?.track === 'audio' ? audioTrimEnd : trimEnd, true)}>
            Go Out
          </Button>
        </div>
      </section>
      {settingsOpen && (
        <div className="modal-backdrop" role="presentation" onMouseDown={() => setSettingsOpen(false)}>
          <section className="settings-modal" role="dialog" aria-modal="true" aria-label="Settings" onMouseDown={(event) => event.stopPropagation()}>
            <div className="modal-header">
              <div className="section-label">
                <Settings2 className="size-4" />
                Settings
              </div>
              <Button size="icon" variant="ghost" onClick={() => setSettingsOpen(false)}>
                x
              </Button>
            </div>
            <div className="modal-grid">
              <div className="inline-field">
                <LabeledInput label="Save Folder" value={settings.saveFolder} onChange={(saveFolder) => setSettings((value) => ({ ...value, saveFolder }))} />
                <Button size="icon" variant="subtle" onClick={browseSaveFolder}>
                  <FolderOpen />
                </Button>
              </div>
              <Button className="w-full" variant="subtle" onClick={openSaveFolder}>
                <FolderOpen />
                Open Save Folder
              </Button>
              <div className="inline-field">
                <LabeledInput label="FFmpeg Path" value={settings.ffmpegPath} onChange={(ffmpegPath) => setSettings((value) => ({ ...value, ffmpegPath }))} />
                <Button size="icon" variant="subtle" onClick={browseFfmpeg}>
                  <FolderOpen />
                </Button>
              </div>
              <div className="space-y-1">
                <Label>Encoder</Label>
                <Select value={settings.exportEncoderKey} onChange={(event) => setSettings((value) => ({ ...value, exportEncoderKey: event.target.value }))}>
                  {encoders.map(([value, label]) => (
                    <option key={value} value={value}>
                      {label}
                    </option>
                  ))}
                </Select>
              </div>
              <div className="space-y-1">
                <Label>Audio Quality</Label>
                <Select
                  value={settings.audioQuality ?? (settings.bestAudio ? 'best' : 'standard')}
                  onChange={(event) => setSettings((value) => ({
                    ...value,
                    audioQuality: event.target.value as 'optimized' | 'best' | 'standard',
                    bestAudio: event.target.value === 'best',
                  }))}
                >
                  <option value="optimized">Optimized (320 kbps AAC compressed - Recommended)</option>
                  <option value="best">Best Audio (24-bit Lossless WAV / 320 kbps MP4)</option>
                  <option value="standard">Standard (160 kbps AAC)</option>
                </Select>
              </div>
              <div className="inline-field">
                <div className="space-y-1">
                  <Label>Audio Device</Label>
                  <Select value={settings.audioDeviceName} onChange={(event) => setSettings((value) => ({ ...value, audioDeviceName: event.target.value }))}>
                    <option value="">System audio (default)</option>
                    {audioDevices.map((device) => (
                      <option key={device} value={device}>
                        {device}
                      </option>
                    ))}
                  </Select>
                </div>
                <Button size="icon" variant="subtle" onClick={scanAudioDevices}>
                  <Gauge />
                </Button>
              </div>
              <LabeledInput label="Record Hotkey" value={settings.recordHotkey} onChange={(recordHotkey) => setSettings((value) => ({ ...value, recordHotkey }))} />
              <LabeledInput label="Reset Hotkey" value={settings.resetHotkey} onChange={(resetHotkey) => setSettings((value) => ({ ...value, resetHotkey }))} />
              <LabeledInput label="GitHub Update URL" value={settings.githubRepositoryUrl} onChange={(githubRepositoryUrl) => setSettings((value) => ({ ...value, githubRepositoryUrl }))} />
              <label className="check-row">
                <input
                  type="checkbox"
                  checked={settings.startWithWindows}
                  onChange={(event) => setSettings((value) => ({ ...value, startWithWindows: event.target.checked }))}
                />
                Start with Windows
              </label>
              <label className="check-row">
                <input
                  type="checkbox"
                  checked={settings.startHiddenInTray}
                  disabled={!settings.startWithWindows}
                  onChange={(event) => setSettings((value) => ({ ...value, startHiddenInTray: event.target.checked }))}
                />
                Start hidden in tray
              </label>
            </div>
            <div className="modal-actions">
              <Button variant="subtle" disabled={isCheckingUpdate} onClick={checkForUpdates}>
                <Upload />
                Update
              </Button>
              <Button variant="primary" onClick={saveSettings}>
                <Settings2 />
                Save Settings
              </Button>
            </div>
          </section>
        </div>
      )}
      <div className="debug-status">{status}</div>
    </main>
  )
}

function TimelineWaveform({
  waveform,
  range,
  gainDb,
  isLoading,
  onBeginEdit,
  onEndEdit,
  onGainChange,
}: {
  waveform: WaveformPeakData | null
  range: { start: number; end: number }
  gainDb: number
  isLoading: boolean
  onBeginEdit: () => void
  onEndEdit: () => void
  onGainChange: (gainDb: number) => void
}) {
  const canvasRef = useRef<HTMLCanvasElement | null>(null)
  const dragRef = useRef<{ startY: number; startGain: number; isDragging: boolean } | null>(null)
  const [cursor, setCursor] = useState('default')

  const finishGainDrag = (event: ReactPointerEvent<HTMLCanvasElement>) => {
    const drag = dragRef.current
    dragRef.current = null
    if (drag?.isDragging) {
      onEndEdit()
    }
    if (event.currentTarget.hasPointerCapture(event.pointerId)) {
      event.currentTarget.releasePointerCapture(event.pointerId)
    }
  }

  useEffect(() => {
    const canvas = canvasRef.current
    const channel = waveform?.channels[0]
    if (!canvas) {
      return
    }
    const rect = canvas.getBoundingClientRect()
    const dpr = window.devicePixelRatio || 1
    canvas.width = Math.max(1, Math.floor(rect.width * dpr))
    canvas.height = Math.max(1, Math.floor(rect.height * dpr))
    const context = canvas.getContext('2d')
    if (!context) {
      return
    }
    context.setTransform(dpr, 0, 0, dpr, 0, 0)
    context.clearRect(0, 0, rect.width, rect.height)
    context.fillStyle = '#203f31'
    context.fillRect(0, 0, rect.width, rect.height)
    const centerY = rect.height / 2
    const gainY = gainToY(gainDb, rect.height)
    context.strokeStyle = '#d6f7e5'
    context.globalAlpha = 0.9
    context.lineWidth = 1
    if (channel && channel.maximums.length > 0) {
      const count = Math.min(channel.minimums.length, channel.maximums.length)
      const gain = dbToLinear(gainDb)
      const amp = Math.max(1, rect.height * 0.47)
      for (let x = 0; x < rect.width; x += 1) {
        const secondA = range.start + (x / Math.max(1, rect.width)) * Math.max(0.001, range.end - range.start)
        const secondB = range.start + ((x + 1) / Math.max(1, rect.width)) * Math.max(0.001, range.end - range.start)
        const indexA = clamp(Math.floor((secondA / Math.max(0.001, waveform.durationSeconds)) * count), 0, count - 1)
        const indexB = clamp(Math.ceil((secondB / Math.max(0.001, waveform.durationSeconds)) * count), indexA + 1, count)
        let minValue = 0
        let maxValue = 0
        for (let index = indexA; index < indexB; index += 1) {
          minValue = Math.min(minValue, channel.minimums[index])
          maxValue = Math.max(maxValue, channel.maximums[index])
        }
        const min = clamp(minValue * gain, -1, 1)
        const max = clamp(maxValue * gain, -1, 1)
        context.beginPath()
        context.moveTo(x + 0.5, centerY - max * amp)
        context.lineTo(x + 0.5, centerY - min * amp)
        context.stroke()
      }
    }
    context.globalAlpha = 1
    context.strokeStyle = 'rgba(255,255,255,0.42)'
    context.beginPath()
    context.moveTo(0, centerY)
    context.lineTo(rect.width, centerY)
    context.stroke()
    context.strokeStyle = '#f4f7fb'
    context.globalAlpha = 0.88
    context.lineWidth = 1.5
    context.beginPath()
    context.moveTo(0, gainY)
    context.lineTo(rect.width, gainY)
    context.stroke()
    context.globalAlpha = 1
  }, [gainDb, range.end, range.start, waveform])

  return (
    <div className="waveform-canvas-wrap">
      <canvas
        aria-label="Source waveform"
        className="waveform-canvas"
        onDoubleClick={(event) => {
          const rect = event.currentTarget.getBoundingClientRect()
          const y = event.clientY - rect.top
          const gainY = gainToY(gainDb, rect.height)
          if (Math.abs(y - gainY) <= 12) {
            event.preventDefault()
            event.stopPropagation()
            onBeginEdit()
            onGainChange(0)
            onEndEdit()
          }
        }}
        onPointerDown={(event) => {
          if (event.button !== 0) {
            return
          }
          const rect = event.currentTarget.getBoundingClientRect()
          const y = event.clientY - rect.top
          const gainY = gainToY(gainDb, rect.height)
          if (Math.abs(y - gainY) > 8) {
            // Clicking above or below line will NOT change audio level!
            // Let the event bubble up to select the audio clip.
            return
          }
          event.preventDefault()
          event.stopPropagation()
          dragRef.current = { startY: y, startGain: gainDb, isDragging: false }
          event.currentTarget.setPointerCapture(event.pointerId)
        }}
        onPointerMove={(event) => {
          const rect = event.currentTarget.getBoundingClientRect()
          const y = event.clientY - rect.top
          const gainY = gainToY(gainDb, rect.height)
          const drag = dragRef.current
          if (!drag) {
            setCursor(Math.abs(y - gainY) <= 8 ? 'ns-resize' : 'default')
            return
          }
          event.preventDefault()
          event.stopPropagation()
          const dy = y - drag.startY
          if (!drag.isDragging) {
            if (Math.abs(dy) >= 3) {
              drag.isDragging = true
              onBeginEdit()
            }
          }
          if (drag.isDragging) {
            const newY = gainToY(drag.startGain, rect.height) + dy
            onGainChange(yToGain(newY, rect.height))
          }
        }}
        onPointerUp={finishGainDrag}
        onPointerCancel={finishGainDrag}
        onLostPointerCapture={finishGainDrag}
        ref={canvasRef}
        style={{ cursor }}
      />
      <div className="waveform-readout">{isLoading ? 'Waveform' : `${gainDb >= 0 ? '+' : ''}${gainDb.toFixed(1)} dB`}</div>
    </div>
  )
}

function RegionSelector() {
  const [start, setStart] = useState<{ x: number; y: number; screenX: number; screenY: number } | null>(null)
  const [current, setCurrent] = useState<{ x: number; y: number; screenX: number; screenY: number } | null>(null)

  useEffect(() => {
    void getCurrentWindow().setFocus()
  }, [])

  const rect = start && current
    ? {
        left: Math.min(start.x, current.x),
        top: Math.min(start.y, current.y),
        width: Math.abs(current.x - start.x),
        height: Math.abs(current.y - start.y),
      }
    : null

  return (
    <main
      className="region-selector"
      onPointerDown={(event) => {
        setStart({ x: event.clientX, y: event.clientY, screenX: event.screenX, screenY: event.screenY })
        setCurrent({ x: event.clientX, y: event.clientY, screenX: event.screenX, screenY: event.screenY })
        event.currentTarget.setPointerCapture(event.pointerId)
      }}
      onPointerMove={(event) => {
        if (start) {
          setCurrent({ x: event.clientX, y: event.clientY, screenX: event.screenX, screenY: event.screenY })
        }
      }}
      onPointerUp={async (event) => {
        if (!start) {
          return
        }
        const end = { x: event.clientX, y: event.clientY, screenX: event.screenX, screenY: event.screenY }
        const width = Math.abs(end.screenX - start.screenX)
        const height = Math.abs(end.screenY - start.screenY)
        if (width > 8 && height > 8) {
          await emit('region-selected', {
            x: Math.min(start.screenX, end.screenX),
            y: Math.min(start.screenY, end.screenY),
            width,
            height,
          })
        }
        if (event.currentTarget.hasPointerCapture(event.pointerId)) {
          event.currentTarget.releasePointerCapture(event.pointerId)
        }
        await getCurrentWindow().close()
      }}
      onKeyDown={async (event) => {
        if (event.key === 'Escape') {
          await getCurrentWindow().close()
        }
      }}
      tabIndex={0}
    >
      <div className="selector-help">Drag to select recording region. Escape cancels.</div>
      {rect && <div className="selector-rect" style={rect} />}
    </main>
  )
}

function NumberField({
  label,
  value,
  step = 1,
  compact = false,
  suffix,
  onChange,
}: {
  label: string
  value: number
  step?: number
  compact?: boolean
  suffix?: string
  onChange: (value: number) => void
}) {
  return (
    <div className={compact ? 'compact-number inline-number' : 'inline-number'}>
      <Label>{label}</Label>
      <Input type="number" value={value} step={step} onChange={(event) => onChange(Number(event.target.value) || 0)} />
      {suffix && <span>{suffix}</span>}
    </div>
  )
}

function LabeledInput({ label, value, onChange }: { label: string; value: string; onChange: (value: string) => void }) {
  return (
    <div className="space-y-1">
      <Label>{label}</Label>
      <Input value={value} onChange={(event) => onChange(event.target.value)} />
    </div>
  )
}

function TrackLabel({ name, detail }: { name: string; detail: string }) {
  return (
    <div className="track-label">
      <Badge>{name}</Badge>
      <span>{detail}</span>
    </div>
  )
}

function CropOverlay({
  crop,
  sourceWidth,
  sourceHeight,
  view,
  forceAspectLock,
  onBeginEdit,
  onEndEdit,
  onChange,
}: {
  crop: Crop
  sourceWidth: number
  sourceHeight: number
  view: ViewerView
  forceAspectLock: boolean
  onBeginEdit: () => void
  onEndEdit: () => void
  onChange: (crop: Crop) => void
}) {
  const layerRef = useRef<HTMLDivElement | null>(null)
  const dragRef = useRef<{ mode: 'move' | 'tl' | 'tr' | 'bl' | 'br'; start: ViewerPoint; crop: Crop } | null>(null)
  const rect = sourceRectToViewport(crop, view)

  const sourcePointFromClient = (clientX: number, clientY: number) => {
    const bounds = layerRef.current?.getBoundingClientRect()
    return viewportToSource({
      x: clientX - (bounds?.left ?? 0),
      y: clientY - (bounds?.top ?? 0),
    }, view)
  }

  const applyDrag = (clientX: number, clientY: number, lockAspect: boolean) => {
    const drag = dragRef.current
    if (!drag) {
      return
    }
    const current = sourcePointFromClient(clientX, clientY)
    const dx = Math.round(current.x - drag.start.x)
    const dy = Math.round(current.y - drag.start.y)
    const c = drag.crop
    const next = (lockAspect || forceAspectLock) && drag.mode !== 'move'
      ? resizeRectWithLockedAspect(c, { width: sourceWidth, height: sourceHeight }, drag.mode, { x: dx, y: dy })
      : drag.mode === 'move'
        ? { ...c, x: c.x + dx, y: c.y + dy }
        : drag.mode === 'tl'
          ? { x: c.x + dx, y: c.y + dy, width: c.width - dx, height: c.height - dy }
          : drag.mode === 'tr'
            ? { x: c.x, y: c.y + dy, width: c.width + dx, height: c.height - dy }
            : drag.mode === 'bl'
              ? { x: c.x + dx, y: c.y, width: c.width - dx, height: c.height + dy }
              : { x: c.x, y: c.y, width: c.width + dx, height: c.height + dy }
    onChange(clampCropForDrag(next, sourceWidth, sourceHeight, drag.mode))
  }

  const beginDrag = (event: ReactPointerEvent<HTMLElement>, mode: 'move' | 'tl' | 'tr' | 'bl' | 'br') => {
    if (event.button !== 0) {
      return
    }
    event.stopPropagation()
    onBeginEdit()
    dragRef.current = { mode, start: sourcePointFromClient(event.clientX, event.clientY), crop }
    event.currentTarget.setPointerCapture(event.pointerId)
  }

  const finishDrag = (event: ReactPointerEvent<HTMLElement>) => {
    if (!dragRef.current) {
      return
    }
    dragRef.current = null
    onEndEdit()
    if (event.currentTarget.hasPointerCapture(event.pointerId)) {
      event.currentTarget.releasePointerCapture(event.pointerId)
    }
  }

  return (
    <div className="crop-layer" ref={layerRef}>
      <div
        className="crop-overlay"
        title="Drag to move. Hold Shift while dragging a corner to lock the current aspect ratio."
        style={{
          left: rect.x,
          top: rect.y,
          width: rect.width,
          height: rect.height,
        }}
        onPointerDown={(event) => beginDrag(event, 'move')}
        onPointerMove={(event) => applyDrag(event.clientX, event.clientY, event.shiftKey)}
        onPointerUp={finishDrag}
        onPointerCancel={finishDrag}
        onLostPointerCapture={finishDrag}
      >
        {(['tl', 'tr', 'bl', 'br'] as const).map((mode) => (
          <i
            className={`crop-handle ${mode}`}
            key={mode}
            onPointerDown={(event) => beginDrag(event, mode)}
            onPointerMove={(event) => applyDrag(event.clientX, event.clientY, event.shiftKey)}
            onPointerUp={finishDrag}
            onPointerCancel={finishDrag}
            onLostPointerCapture={finishDrag}
          />
        ))}
      </div>
    </div>
  )
}

function mergeCuts(cuts: CutRange[]) {
  const ordered = [...cuts].sort((a, b) => a.start - b.start)
  const merged: CutRange[] = []
  for (const cut of ordered) {
    if (cut.end <= cut.start) {
      continue
    }
    const previous = merged[merged.length - 1]
    if (!previous || cut.start > previous.end) {
      merged.push(cut)
    } else {
      previous.end = Math.max(previous.end, cut.end)
    }
  }
  return merged
}

function cloneCuts(cuts: CutRange[]) {
  return cuts.map((cut) => ({ ...cut }))
}

function editSnapshotsEqual(left: EditSnapshot, right: EditSnapshot) {
  return left.trimStart === right.trimStart
    && left.trimEnd === right.trimEnd
    && left.timelineOffset === right.timelineOffset
    && left.audioTrimStart === right.audioTrimStart
    && left.audioTrimEnd === right.audioTrimEnd
    && left.audioTimelineOffset === right.audioTimelineOffset
    && left.outputWidth === right.outputWidth
    && left.outputHeight === right.outputHeight
    && left.autoFit720 === right.autoFit720
    && left.audioGainDb === right.audioGainDb
    && left.crop.x === right.crop.x
    && left.crop.y === right.crop.y
    && left.crop.width === right.crop.width
    && left.crop.height === right.crop.height
    && cutsEqual(left.cuts, right.cuts)
    && cutsEqual(left.audioCuts, right.audioCuts)
}

function cutsEqual(left: CutRange[], right: CutRange[]) {
  return left.length === right.length
    && left.every((cut, index) => cut.start === right[index].start && cut.end === right[index].end)
}

function cutEditBounds(cuts: CutRange[], index: number, mode: 'cut-start' | 'cut-end', trimStart: number, trimEnd: number) {
  const cut = cuts[index]
  if (!cut) {
    return { min: trimStart, max: trimEnd }
  }
  if (mode === 'cut-start') {
    const previous = cuts[index - 1]
    return {
      min: previous ? previous.end + 0.033 : trimStart,
      max: cut.end - 0.033,
    }
  }
  const next = cuts[index + 1]
  return {
    min: cut.start + 0.033,
    max: next ? next.start - 0.033 : trimEnd,
  }
}

function normalizePlayableSourceTime(value: number, cuts: CutRange[], trimStart: number, trimEnd: number) {
  let next = clamp(value, trimStart, trimEnd)
  for (const cut of [...cuts].sort((a, b) => a.start - b.start)) {
    if (next >= cut.start && next < cut.end) {
      next = cut.end
    }
  }
  return clamp(next, trimStart, trimEnd)
}

function nextPlayableSourceTime(previous: number, rawNext: number, cuts: CutRange[], trimStart: number, trimEnd: number) {
  let next = clamp(rawNext, trimStart, trimEnd)
  for (const cut of [...cuts].sort((a, b) => a.start - b.start)) {
    if (cut.end <= trimStart || cut.start >= trimEnd) {
      continue
    }
    const cutStart = clamp(cut.start, trimStart, trimEnd)
    const cutEnd = clamp(cut.end, trimStart, trimEnd)
    if (previous <= cutStart && next >= cutStart) {
      next = cutEnd + Math.max(0, next - cutStart)
    } else if (next >= cutStart && next < cutEnd) {
      next = cutEnd
    }
  }
  return clamp(next, trimStart, trimEnd)
}

function buildDisplayClips(start: number, end: number, cuts: CutRange[], offset = 0) {
  const ordered = [...cuts].sort((a, b) => a.start - b.start)
  const segments: DisplayClip[] = []
  let cursor = start
  let timelineCursor = Math.max(0, offset)
  for (const cut of ordered) {
    const cutStart = clamp(cut.start, start, end)
    const cutEnd = clamp(cut.end, start, end)
    if (cutEnd <= cutStart) {
      continue
    }
    if (cutStart > cursor) {
      const length = cutStart - cursor
      segments.push({ start: timelineCursor, end: timelineCursor + length, sourceStart: cursor, sourceEnd: cutStart })
      timelineCursor += length
    }
    cursor = Math.max(cursor, cutEnd)
  }
  if (cursor < end) {
    const length = end - cursor
    segments.push({ start: timelineCursor, end: timelineCursor + length, sourceStart: cursor, sourceEnd: end })
  }
  return segments
}

function sourceToTimelineTime(sourceSeconds: number, clips: DisplayClip[]) {
  if (clips.length === 0) {
    return 0
  }
  for (const clip of clips) {
    if (sourceSeconds >= clip.sourceStart && sourceSeconds <= clip.sourceEnd) {
      return clip.start + (sourceSeconds - clip.sourceStart)
    }
  }
  const previous = [...clips].reverse().find((clip) => sourceSeconds >= clip.sourceEnd)
  if (previous) {
    return previous.end
  }
  return clips[0].start
}

function timelineToSourceTime(timelineSeconds: number, clips: DisplayClip[]) {
  if (clips.length === 0) {
    return 0
  }
  for (const clip of clips) {
    if (timelineSeconds >= clip.start && timelineSeconds <= clip.end) {
      return clip.sourceStart + (timelineSeconds - clip.start)
    }
  }
  const previous = [...clips].reverse().find((clip) => timelineSeconds >= clip.end)
  if (previous) {
    return previous.sourceEnd
  }
  return clips[0].sourceStart
}

function timelineHasClipAt(timelineSeconds: number, clips: DisplayClip[]) {
  return clips.some((clip) => timelineSeconds >= clip.start && timelineSeconds <= clip.end)
}

function sourceEndForKeptDuration(clips: DisplayClip[], maxSeconds: number) {
  if (clips.length === 0) {
    return 0
  }
  let remaining = Math.max(0, maxSeconds)
  for (const clip of clips) {
    const length = Math.max(0, clip.end - clip.start)
    if (remaining <= length) {
      return clip.sourceStart + remaining
    }
    remaining -= length
  }
  return clips[clips.length - 1].sourceEnd
}

function uniqueClipSelections(selections: ClipSelection[]) {
  const seen = new Set<string>()
  return selections.filter((selection) => {
    const key = `${selection.track}:${selection.index}`
    if (seen.has(key)) {
      return false
    }
    seen.add(key)
    return true
  })
}

function marqueeSelectionStyle(selection: NonNullable<MarqueeSelection>) {
  const left = Math.min(selection.startX, selection.currentX)
  const top = Math.min(selection.startY, selection.currentY)
  return {
    left: `${left}px`,
    top: `${top}px`,
    width: `${Math.abs(selection.currentX - selection.startX)}px`,
    height: `${Math.abs(selection.currentY - selection.startY)}px`,
  }
}

function clampCrop(crop: Crop, sourceWidth: number, sourceHeight: number) {
  return clampCropRect(crop, { width: sourceWidth, height: sourceHeight })
}

function clampCropForDrag(crop: Crop, sourceWidth: number, sourceHeight: number, mode: 'move' | 'tl' | 'tr' | 'bl' | 'br') {
  return clampCropRect(crop, { width: sourceWidth, height: sourceHeight }, mode)
}

function cropForAspect(sourceWidth: number, sourceHeight: number, aspect: number) {
  const sourceAspect = sourceWidth / Math.max(1, sourceHeight)
  if (sourceAspect > aspect) {
    const width = makeEven(sourceHeight * aspect)
    return clampCrop({ x: Math.round((sourceWidth - width) / 2), y: 0, width, height: sourceHeight }, sourceWidth, sourceHeight)
  }
  const height = makeEven(sourceWidth / aspect)
  return clampCrop({ x: 0, y: Math.round((sourceHeight - height) / 2), width: sourceWidth, height }, sourceWidth, sourceHeight)
}

function makeEven(value: number) {
  const rounded = Math.max(2, Math.round(value))
  return rounded % 2 === 0 ? rounded : rounded - 1
}

function makeOutputDimension(value: number) {
  return clamp(makeEven(value), 2, 16384)
}

function timelinePercent(secondsAt: number, viewport: { start: number; span: number }) {
  return ((secondsAt - viewport.start) / Math.max(0.001, viewport.span)) * 100
}

function timelineRangeStyle(start: number, end: number, viewport: { start: number; span: number }) {
  const left = timelinePercent(start, viewport)
  return {
    left: `${left}%`,
    width: `${Math.max(0, ((end - start) / Math.max(0.001, viewport.span)) * 100)}%`,
  }
}

function timelineDurationFor(clipDuration: number, keptSeconds: number, timelineOffset: number, audioKeptSeconds = 0, audioTimelineOffset = 0) {
  return Math.max(1, Math.max(clipDuration, timelineOffset + keptSeconds, audioTimelineOffset + audioKeptSeconds) + 30)
}

function dbToLinear(db: number) {
  return 10 ** (clamp(db, -24, 36) / 20)
}

function gainToY(db: number, height: number) {
  return height - ((clamp(db, -24, 36) + 24) / 60) * height
}

function yToGain(y: number, height: number) {
  return clamp((1 - clamp(y, 0, height) / Math.max(1, height)) * 60 - 24, -24, 36)
}

function clamp(value: number, min: number, max: number) {
  return Math.min(max, Math.max(min, value))
}

function fileName(path: string) {
  return path.split(/[\\/]/).pop() ?? path
}

function joinPath(folder: string, name: string) {
  const clean = folder.trim().replace(/[\\/]+$/, '')
  if (!clean) {
    return name
  }
  return `${clean}\\${name}`
}

function formatTime(value: number) {
  const safe = Math.max(0, value)
  const minutes = Math.floor(safe / 60)
  const seconds = Math.floor(safe % 60)
  const millis = Math.floor((safe % 1) * 1000)
  return `${minutes.toString().padStart(2, '0')}:${seconds.toString().padStart(2, '0')}.${millis.toString().padStart(3, '0')}`
}

function formatBytes(bytes: number) {
  return `${(bytes / 1024 / 1024).toFixed(2)} MB`
}

function dateStamp() {
  const now = new Date()
  const pad = (value: number) => value.toString().padStart(2, '0')
  return `${now.getFullYear()}${pad(now.getMonth() + 1)}${pad(now.getDate())}-${pad(now.getHours())}${pad(now.getMinutes())}${pad(now.getSeconds())}`
}

function shortError(error: unknown) {
  const message = error instanceof Error ? error.message : String(error)
  return message.split('\n').filter(Boolean)[0]?.slice(0, 180) || 'Operation failed'
}

function isInterruptedPlaybackError(error: unknown) {
  const message = error instanceof Error ? error.message : String(error)
  return message.includes('play() request was interrupted') || message.includes('The play() request was interrupted')
}

function canUseTauri() {
  return typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window
}

function nextPaint() {
  return new Promise<void>((resolve) => {
    requestAnimationFrame(() => setTimeout(resolve, 0))
  })
}

function tauriInvoke<T = unknown>(command: string, args?: Record<string, unknown>) {
  if (!canUseTauri()) {
    return Promise.reject(new Error('Run inside the Tauri app for file and FFmpeg commands.'))
  }
  return invoke<T>(command, args)
}

async function notify(title: string, body: string) {
  if (!canUseTauri()) {
    return
  }
  let granted = await isPermissionGranted()
  if (!granted) {
    granted = (await requestPermission()) === 'granted'
  }
  if (granted) {
    sendNotification({ title, body })
  }
}

export default App
