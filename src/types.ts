export type AppSettings = {
  saveFolder: string
  ffmpegPath: string
  frameRate: number
  captureBackend: 'desktopDuplication' | 'gdi'
  maxMegabytes: number
  sizeCapEnabled: boolean
  qualityTargetKbps: number
  exportEncoderKey: string
  exportBitrateScale: number
  audioGainDb: number
  unsupportedEncoderKeys: string[]
  encoderBenchmarks: EncoderBenchmark[]
  includeVideo: boolean
  includeAudio: boolean
  bestAudio: boolean
  audioQuality: 'optimized' | 'best' | 'standard'
  audioDeviceName: string
  startWithWindows: boolean
  startHiddenInTray: boolean
  recordHotkey: string
  resetHotkey: string
  githubRepositoryUrl: string
}

export type EncoderBenchmark = {
  encoderKey: string
  encoderLabel: string
  seconds: number
  bytes: number
  testedAt: string
}

export type VideoInfo = {
  id: string
  path: string
  originalPath: string
  width: number
  height: number
  durationSeconds: number
}

export type PreviewCache = {
  folder: string
  fps: number
  frames: string[]
}

export type WaveformPeakData = {
  durationSeconds: number
  sampleRate: number
  channels: WaveformPeakChannel[]
}

export type WaveformPeakChannel = {
  minimums: number[]
  maximums: number[]
}

export type Crop = {
  x: number
  y: number
  width: number
  height: number
}

export type CutRange = {
  start: number
  end: number
}

export type ExportRequest = {
  inputPath: string
  outputPath: string
  start: number
  end: number
  audioStart: number
  audioEnd: number
  crop: Crop
  outputWidth: number
  outputHeight: number
  autoFit720: boolean
  cuts: CutRange[]
  audioCuts: CutRange[]
  settings: AppSettings
}

export type ExportResult = {
  path: string
  bytes: number
  seconds: number
}

export type BenchmarkResult = {
  encoderKey: string
  encoderLabel: string
  path: string
  bytes: number
  seconds: number
  success: boolean
  error: string
}

export type RecordingRequest = {
  x: number
  y: number
  width: number
  height: number
  settings: AppSettings
}
