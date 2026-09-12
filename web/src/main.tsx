import { StrictMode, useEffect, useRef, useState } from 'react'
import { createRoot } from 'react-dom/client'
import { BrowserRouter, Navigate, Route, Routes } from 'react-router'
import './index.css'

type Settings = { password: string; address: string }
type Codec = 'h264' | 'h265' | 'av1'
type HardwareCodecs = { h264: boolean; h265: boolean; av1: boolean }
type StreamSettings = { codec: Codec; resolution: number; bitrate: number; host_cursor_visible: boolean }
const ADMIN_TOKEN = 'beam_admin_token'
const STREAM_SETTINGS = 'beam_stream_settings'

function loadStreamSettings(): Partial<StreamSettings> {
  try {
    const value = JSON.parse(localStorage.getItem(STREAM_SETTINGS) || '{}') as Partial<StreamSettings>
    return {
      ...(value.resolution && [0.25, 0.5, 0.75, 1].includes(value.resolution) ? { resolution: value.resolution } : {}),
      ...(value.codec === 'h264' || value.codec === 'h265' || value.codec === 'av1' ? { codec: value.codec } : {}),
      ...(value.bitrate && value.bitrate >= 1_000_000 && value.bitrate <= 200_000_000 ? { bitrate: value.bitrate } : {}),
      ...(typeof value.host_cursor_visible === 'boolean' ? { host_cursor_visible: value.host_cursor_visible } : {}),
    }
  } catch {
    return {}
  }
}

class HttpError extends Error {
  constructor(readonly status: number, message: string) {
    super(message)
  }
}

async function json<T>(path: string, init?: RequestInit): Promise<T> {
  const headers = new Headers(init?.headers)
  headers.set('content-type', 'application/json')
  const response = await fetch(path, {
    ...init,
    credentials: 'same-origin',
    headers,
  })
  if (!response.ok) throw new HttpError(response.status, (await response.text()) || response.statusText)
  return response.json()
}

function Shell({ children }: { children: React.ReactNode }) {
  return (
    <main className="min-h-screen bg-[radial-gradient(circle_at_top,#172235_0,#080b10_42%)] px-5 py-10">
      <div className="mx-auto max-w-5xl">
        <header className="mb-10 flex items-center gap-3">
          <div className="grid size-9 place-items-center rounded-xl bg-cyan-300 font-black text-slate-950">B</div>
          <span className="text-lg font-semibold tracking-tight">Beam</span>
        </header>
        {children}
      </div>
    </main>
  )
}

function Viewer() {
  const video = useRef<HTMLVideoElement>(null)
  const player = useRef<HTMLElement>(null)
  const peer = useRef<RTCPeerConnection | null>(null)
  const connectionGeneration = useRef(0)
  const latencyFrameCallback = useRef<number | undefined>(undefined)
  const inputChannel = useRef<RTCDataChannel | null>(null)
  const settingsReady = useRef(false)
  const reconnecting = useRef(false)
  const keyboardCleanup = useRef<(() => void) | null>(null)
  const rememberedSettings = useRef(loadStreamSettings())
  const hostSettings = useRef<StreamSettings | null>(null)
  const latencySamples = useRef<Array<{ time: number; value: number }>>([])
  const [password, setPassword] = useState('')
  const [status, setStatus] = useState('Ready')
  const [latency, setLatency] = useState<number | null>(null)
  const [fullscreen, setFullscreen] = useState(false)
  const [streamAspect, setStreamAspect] = useState(16 / 9)
  const [clientMouseVisible, setClientMouseVisible] = useState(true)
  const [resolution, setResolution] = useState<number | null>(null)
  const [codec, setCodec] = useState<Codec | null>(null)
  const [bitrate, setBitrate] = useState<number | null>(null)
  const [hostMouseVisible, setHostMouseVisible] = useState<boolean | null>(null)
  const [hardwareCodecs, setHardwareCodecs] = useState<HardwareCodecs | null>(null)
  async function toggleFullscreen() {
    const fullscreenDocument = document as Document & {
      webkitFullscreenElement?: Element
      webkitExitFullscreen?: () => Promise<void> | void
    }
    const fullscreenElement = document.fullscreenElement ?? fullscreenDocument.webkitFullscreenElement
    if (fullscreenElement) {
      if (document.exitFullscreen) await document.exitFullscreen()
      else await fullscreenDocument.webkitExitFullscreen?.()
      unlockKeyboard()
    } else {
      const element = player.current
      if (!element) return
      const fullscreenElement = element as HTMLElement & {
        webkitRequestFullscreen?: () => Promise<void> | void
      }
      try {
        await (element.requestFullscreen as (options?: { keyboardLock?: 'browser' }) => Promise<void>)?.({
          keyboardLock: 'browser',
        })
      } catch {
        if (element.requestFullscreen) await element.requestFullscreen()
        else await fullscreenElement.webkitRequestFullscreen?.()
      }
      const keyboard = (navigator as Navigator & {
        keyboard?: { lock?: (keys?: string[]) => Promise<void> }
      }).keyboard
      try {
        await keyboard?.lock?.(['Escape'])
      } catch {
        // Keyboard Lock is not available in every browser.
      }
    }
  }

  useEffect(() => {
    const update = () => {
      const fullscreenDocument = document as Document & { webkitFullscreenElement?: Element }
      const fullscreenElement = document.fullscreenElement ?? fullscreenDocument.webkitFullscreenElement
      const playerIsFullscreen = fullscreenElement === player.current
      setFullscreen(playerIsFullscreen)
      if (!fullscreenElement) unlockKeyboard()
    }
    document.addEventListener('fullscreenchange', update)
    document.addEventListener('webkitfullscreenchange', update)
    return () => {
      document.removeEventListener('fullscreenchange', update)
      document.removeEventListener('webkitfullscreenchange', update)
    }
  }, [])

  /* Fix for dumbass Chrome MacOS bug where native green "Exit Fullscreen" button doesn't actually make the page exit fullscreen mode */
  useEffect(() => {
    let lastWidth = window.innerWidth
    let lastHeight = window.innerHeight
    let inFullscreen = !!document.fullscreenElement || document.fullscreen
    let exitTimer: number | undefined
    const fullscreenPoll = window.setInterval(() => {
      const isFullscreen = !!document.fullscreenElement || document.fullscreen
      if (inFullscreen && isFullscreen && (lastWidth !== window.innerWidth || lastHeight !== window.innerHeight)) {
        exitTimer = window.setTimeout(() => {
          void document.exitFullscreen()
          void document.body.requestFullscreen()
          void document.exitFullscreen()
        }, 900)
      }
      lastWidth = window.innerWidth
      lastHeight = window.innerHeight
      inFullscreen = isFullscreen
    }, 500)
    return () => {
      window.clearInterval(fullscreenPoll)
      if (exitTimer !== undefined) window.clearTimeout(exitTimer)
    }
  }, [])

  function unlockKeyboard() {
    const keyboard = (navigator as Navigator & {
      keyboard?: { unlock?: () => void }
    }).keyboard
    keyboard?.unlock?.()
  }

  async function connect(event?: React.FormEvent) {
    event?.preventDefault()
    const generation = ++connectionGeneration.current
    if (latencyFrameCallback.current !== undefined) video.current?.cancelVideoFrameCallback(latencyFrameCallback.current)
    latencyFrameCallback.current = undefined
    latencySamples.current = []
    setLatency(null)
    setStatus('Authenticating…')
    try {
      await json('/api/session', { method: 'POST', body: JSON.stringify({ password }) })
      const pc = new RTCPeerConnection({ iceServers: [] })
      keyboardCleanup.current?.()
      peer.current?.close()
      peer.current = pc
      pc.addTransceiver('video', { direction: 'recvonly' })
      pc.addTransceiver('audio', { direction: 'recvonly' })
      const input = pc.createDataChannel('input')
      inputChannel.current = input
      input.onmessage = event => {
        if (generation !== connectionGeneration.current) return
        try {
          const message = JSON.parse(event.data) as {
            type?: string; codec?: Codec; resolution?: number; bitrate?: number; host_cursor_visible?: boolean; hardware_codecs?: HardwareCodecs
          }
          if (message.type !== 'streamSettings' || message.codec == null || message.resolution == null || message.bitrate == null || message.host_cursor_visible == null) return
          const changed = settingsReady.current && hostSettings.current != null && (
            hostSettings.current.codec !== message.codec ||
            hostSettings.current.resolution !== message.resolution ||
            hostSettings.current.bitrate !== message.bitrate ||
            hostSettings.current.host_cursor_visible !== message.host_cursor_visible
          )
          const nextSettings = {
            codec: message.codec,
            resolution: message.resolution,
            bitrate: message.bitrate,
            host_cursor_visible: message.host_cursor_visible,
          }
          hostSettings.current = nextSettings
          rememberedSettings.current = nextSettings
          localStorage.setItem(STREAM_SETTINGS, JSON.stringify(nextSettings))
          setResolution(message.resolution)
          setCodec(message.codec)
          setBitrate(message.bitrate)
          setHostMouseVisible(message.host_cursor_visible)
          setHardwareCodecs(message.hardware_codecs ?? { h264: false, h265: false, av1: false })
          settingsReady.current = true
          if (changed && !reconnecting.current) {
            reconnecting.current = true
            pc.close()
            window.setTimeout(() => {
              if (generation !== connectionGeneration.current) return
              reconnecting.current = false
              settingsReady.current = false
              void connect()
            }, 100)
          }
        } catch {
          // Ignore non-control messages.
        }
      }
      input.onopen = () => {
        if (generation !== connectionGeneration.current) return
        const settings = rememberedSettings.current
        if (settings.resolution == null && settings.bitrate == null && settings.host_cursor_visible == null) return
        input.send(JSON.stringify({
          type: 'setStreamSettings',
          ...settings,
        }))
      }
      const sendMouseButton = (event: PointerEvent, down: boolean) => {
        if (event.pointerType !== 'mouse' || input.readyState !== 'open') return
        video.current?.focus()
        if (document.fullscreenElement !== player.current) return
        event.preventDefault()
        if ([0, 1, 2].includes(event.button))
          input.send(JSON.stringify({ type: 'mouseButton', button: event.button, down }))
      }
      const sendKey = (type: 'keyDown' | 'keyUp', event: KeyboardEvent) => {
        if (document.fullscreenElement !== player.current || event.isComposing || input.readyState !== 'open') return
        event.preventDefault()
        input.send(JSON.stringify({ type, code: event.code, key: event.key }))
      }
      const keyDown = (event: KeyboardEvent) => sendKey('keyDown', event)
      const keyUp = (event: KeyboardEvent) => sendKey('keyUp', event)
      window.addEventListener('keydown', keyDown, true)
      window.addEventListener('keyup', keyUp, true)
      keyboardCleanup.current = () => {
        window.removeEventListener('keydown', keyDown, true)
        window.removeEventListener('keyup', keyUp, true)
      }
      pc.ontrack = ({ receiver, streams: [stream] }) => {
        const lowLatency = receiver as RTCRtpReceiver & {
          jitterBufferTarget?: number
          playoutDelayHint?: number
        }
        try {
          lowLatency.jitterBufferTarget = 0
          lowLatency.playoutDelayHint = 0
        } catch {
          // These latency hints are not implemented by every browser.
        }
        if (video.current) {
          video.current.srcObject = stream
          const currentVideo = video.current
          const recordLatency = (value: number) => {
            const time = performance.now()
            const samples = latencySamples.current
            samples.push({ time, value: Math.max(0, value) })
            const recent = samples.filter(sample => sample.time >= time - 1000)
            latencySamples.current = recent
            setLatency(Math.max(1, Math.round(recent.reduce((sum, sample) => sum + sample.value, 0) / recent.length)))
          }
          const updateLatency = (_now: number, metadata: VideoFrameCallbackMetadata & { captureTime?: number; receiveTime?: number }) => {
            if (generation !== connectionGeneration.current) return
            void measureFrameAge(pc, performance.now(), metadata).then(value => {
              if (generation === connectionGeneration.current && value != null) recordLatency(value)
            })
            latencyFrameCallback.current = currentVideo.requestVideoFrameCallback(updateLatency)
          }
          if ('requestVideoFrameCallback' in currentVideo)
            latencyFrameCallback.current = currentVideo.requestVideoFrameCallback(updateLatency)
          video.current.onloadedmetadata = () => {
            if (video.current?.videoWidth && video.current.videoHeight)
              setStreamAspect(video.current.videoWidth / video.current.videoHeight)
          }
          video.current.onpointermove = event => {
            if (event.pointerType && event.pointerType !== 'mouse') return
            if (document.fullscreenElement !== player.current) return
            const position = streamPosition(video.current!, event.clientX, event.clientY)
            if (position && input.readyState === 'open')
              input.send(JSON.stringify({ type: 'mouseMove', ...position }))
          }
          video.current.onwheel = event => {
            if (document.fullscreenElement !== player.current) return
            event.preventDefault()
            if (input.readyState === 'open')
              input.send(JSON.stringify({
                type: 'wheel', deltaX: event.deltaX, deltaY: event.deltaY, deltaMode: event.deltaMode,
              }))
          }
          video.current.onpointerdown = event => sendMouseButton(event, true)
          video.current.onpointerup = event => sendMouseButton(event, false)
          video.current.oncontextmenu = event => event.preventDefault()
        }
      }
      pc.onconnectionstatechange = () => {
        if (generation !== connectionGeneration.current) return
        setStatus(pc.connectionState)
        if (pc.connectionState !== 'connected') {
          if (latencyFrameCallback.current !== undefined) video.current?.cancelVideoFrameCallback(latencyFrameCallback.current)
          latencyFrameCallback.current = undefined
          latencySamples.current = []
          setLatency(null)
        }
        if (['disconnected', 'failed', 'closed'].includes(pc.connectionState) && video.current)
          video.current.srcObject = null
        if (['disconnected', 'failed', 'closed'].includes(pc.connectionState)) {
          keyboardCleanup.current?.()
          settingsReady.current = false
          if (!reconnecting.current) {
            reconnecting.current = true
            window.setTimeout(() => {
              if (generation !== connectionGeneration.current) return
              reconnecting.current = false
              void connect()
            }, 250)
          }
        }
      }
      await pc.setLocalDescription(await pc.createOffer())
      await waitForIce(pc)
      const answer = await json<RTCSessionDescriptionInit>('/api/offer', {
        method: 'POST', body: JSON.stringify({ sdp: pc.localDescription?.sdp }),
      })
      await pc.setRemoteDescription(answer)
    } catch (error) {
      setStatus(error instanceof Error ? error.message : 'Connection failed')
    }
  }

  useEffect(() => () => {
    connectionGeneration.current++
    if (latencyFrameCallback.current !== undefined) video.current?.cancelVideoFrameCallback(latencyFrameCallback.current)
    keyboardCleanup.current?.()
    peer.current?.close()
  }, [])

  function sendStreamSettings(next: Partial<StreamSettings>) {
    if (codec == null || resolution == null || bitrate == null || hostMouseVisible == null) return
    const settings = {
      codec: next.codec ?? codec,
      resolution: next.resolution ?? resolution,
      bitrate: next.bitrate ?? bitrate,
      host_cursor_visible: next.host_cursor_visible ?? hostMouseVisible,
    }
    rememberedSettings.current = settings
    setCodec(settings.codec)
    setResolution(settings.resolution)
    setBitrate(settings.bitrate)
    setHostMouseVisible(settings.host_cursor_visible)
    localStorage.setItem(STREAM_SETTINGS, JSON.stringify(settings))
    if (!inputChannel.current || inputChannel.current.readyState !== 'open') return
    inputChannel.current.send(JSON.stringify({
      type: 'setStreamSettings',
      ...settings,
    }))
  }

  return (
    <Shell>
      <section ref={player} style={{ cursor: clientMouseVisible ? 'default' : 'none' }} className="relative overflow-hidden border border-white/10 bg-black shadow-2xl shadow-cyan-950/20">
        <video ref={video} tabIndex={0} controls={false} autoPlay playsInline onContextMenu={event => event.preventDefault()} onClick={() => {
          video.current?.focus()
          void video.current?.play()
          if (!document.fullscreenElement) void toggleFullscreen()
        }}
          style={{ aspectRatio: streamAspect }} className="w-full bg-black object-contain" />
        {!fullscreen && <button type="button" onClick={() => void toggleFullscreen()}
          aria-label={fullscreen ? 'Exit fullscreen' : 'Enter fullscreen'}
          className="absolute right-3 top-3 rounded-lg bg-black/60 px-3 py-2 text-sm font-medium text-white backdrop-blur hover:bg-black/80">
          {fullscreen ? 'Exit fullscreen' : 'Fullscreen'}
        </button>}
      </section>
      {!fullscreen && codec != null && resolution != null && bitrate != null && hostMouseVisible != null && <section className="mt-4 flex flex-wrap items-center gap-3 rounded-2xl border border-white/10 bg-white/[.04] p-4 text-sm">
        <button type="button" role="switch" aria-checked={clientMouseVisible} onClick={() => setClientMouseVisible(value => !value)}
          className="rounded-lg border border-white/10 px-3 py-2 hover:bg-white/10">
          Client cursor: {clientMouseVisible ? 'visible' : 'hidden'}
        </button>
        <button type="button" role="switch" aria-checked={hostMouseVisible}
          onClick={() => sendStreamSettings({ host_cursor_visible: !hostMouseVisible })}
          className="rounded-lg border border-white/10 px-3 py-2 hover:bg-white/10 disabled:opacity-50">
          Host cursor: {hostMouseVisible ? 'visible' : 'hidden'}
        </button>
        <label className="flex items-center gap-2">
          Codec
          <select value={codec} onChange={event => sendStreamSettings({ codec: event.target.value as Codec })}
            className="rounded-lg border border-white/10 bg-slate-900 px-2 py-2">
            <option value="h264">H.264{hardwareCodecs && !hardwareCodecs.h264 ? ' (slow)' : ''}</option>
            <option value="h265">H.265{hardwareCodecs && !hardwareCodecs.h265 ? ' (slow)' : ''}</option>
            <option value="av1">AV1{hardwareCodecs && !hardwareCodecs.av1 ? ' (slow)' : ''}</option>
          </select>
        </label>
        <label className="flex items-center gap-2">
          Resolution
          <select value={resolution}
            onChange={event => sendStreamSettings({ resolution: Number(event.target.value) })}
            className="rounded-lg border border-white/10 bg-slate-900 px-2 py-2">
            {[0.25, 0.5, 0.75, 1].map(value => <option key={value} value={value}>{value}x</option>)}
          </select>
        </label>
        <label className="flex items-center gap-2">
          Bitrate
          <select value={bitrate / 1_000_000}
            onChange={event => sendStreamSettings({ bitrate: Number(event.target.value) * 1_000_000 })}
            className="rounded-lg border border-white/10 bg-slate-900 px-2 py-2">
            {[1, 5, 10, 15, 20, 25, 30, 40, 50, 60, 80, 100, 150, 200].map(value => <option key={value} value={value}>{value} Mbps</option>)}
          </select>
        </label>
      </section>}
      <form onSubmit={connect} className="mt-6 flex flex-col gap-3 sm:flex-row">
        <input aria-label="Host password" type="password" value={password} onChange={e => setPassword(e.target.value)}
          placeholder="Host password" className="min-w-0 flex-1 rounded-xl border border-white/10 bg-white/5 px-4 py-3 outline-none focus:border-cyan-300/60" />
        <button className="rounded-xl bg-cyan-300 px-6 py-3 font-semibold text-slate-950 hover:bg-cyan-200">Connect</button>
      </form>
      <p className="mt-3 text-sm text-slate-400">
        {status}
        {status === 'connected' && latency != null && <span className="ml-2 text-slate-500">· {latency} ms</span>}
      </p>
    </Shell>
  )
}

function SettingsPage() {
  const [token] = useState(() => location.hash.slice(1) || localStorage.getItem(ADMIN_TOKEN) || '')
  const [authorized, setAuthorized] = useState<boolean | null>(null)
  const [password, setPassword] = useState('')
  const [address, setAddress] = useState('')
  const [showPassword, setShowPassword] = useState(false)
  const [stopping, setStopping] = useState(false)
  const [status, setStatus] = useState('Loading…')
  const [logs, setLogs] = useState('')

  useEffect(() => {
    history.replaceState(null, '', location.pathname + location.search)
    if (!token) {
      setAuthorized(false)
      return
    }
    json<Settings>('/api/admin/settings', { headers: { authorization: `Bearer ${token}` } })
      .then(value => {
        localStorage.setItem(ADMIN_TOKEN, token)
        setPassword(value.password)
        setAddress(value.address)
        setStatus('Saved.')
        setAuthorized(true)
        return json('/api/admin/settings-opened', {
          method: 'POST', headers: { authorization: `Bearer ${token}` },
        })
      })
      .catch(error => {
        if (error instanceof HttpError && [401, 403].includes(error.status)) {
          localStorage.removeItem(ADMIN_TOKEN)
          setAuthorized(false)
        } else {
          setStatus(error instanceof Error ? error.message : 'Unavailable')
        }
      })
  }, [token])

  useEffect(() => {
    if (authorized !== true) return
    let active = true
    const refresh = () => {
      void json<{ logs: string }>('/api/admin/logs', {
        headers: { authorization: `Bearer ${token}` },
      }).then(value => {
        if (active) setLogs(value.logs)
      }).catch(() => {})
    }
    refresh()
    const timer = window.setInterval(refresh, 2000)
    return () => {
      active = false
      window.clearInterval(timer)
    }
  }, [authorized, token])

  async function save(event: React.FormEvent) {
    event.preventDefault()
    setStatus('Saving…')
    try {
      await json('/api/admin/settings', {
        method: 'PUT',
        headers: { authorization: `Bearer ${token}` },
        body: JSON.stringify({ password }),
      })
      setStatus('Saved')
    } catch (error) {
      setStatus(error instanceof Error ? error.message : 'Save failed')
    }
  }

  async function shutdown() {
    if (!confirm('Stop Beam server?')) return
    setStopping(true)
    try {
      await json('/api/admin/shutdown', { method: 'POST', headers: { authorization: `Bearer ${token}` } })
      setStatus('You can now close this tab')
      window.close()
    } catch (error) {
      setStatus(error instanceof Error ? error.message : 'Could not stop Beam')
      setStopping(false)
    }
  }

  if (authorized !== true) return (
    <Shell>
      <section className="max-w-xl rounded-3xl border border-white/10 bg-white/[.04] p-7 shadow-2xl">
        <h1 className="text-3xl font-semibold tracking-tight">
          {authorized === false ? 'Browser isn’t authorised' : 'Opening settings…'}
        </h1>
        <p className="mt-3 text-sm leading-6 text-slate-400">
          {authorized === false ? 'Use “Open Beam Settings” in the Beam tray menu.' : status}
        </p>
      </section>
    </Shell>
  )

  return (
    <Shell>
      <section className="max-w-xl rounded-3xl border border-white/10 bg-white/[.04] p-7 shadow-2xl">
        <p className="mb-2 text-xs font-semibold uppercase tracking-[.2em] text-cyan-300">Host settings</p>
        <h1 className="text-3xl font-semibold tracking-tight">Who can connect?</h1>
        <p className="mt-4 rounded-xl bg-black/20 px-4 py-3 text-sm text-slate-300">
          Devices on your network can connect to this computer at <a className="font-mono text-cyan-300 hover:underline" href={address}>{address}</a>
        </p>
        <form onSubmit={save} className="mt-8">
          <label className="mb-2 block text-sm font-medium" htmlFor="password">Client password</label>
          <div className="relative">
            <input id="password" type={showPassword ? 'text' : 'password'} value={password}
              onChange={e => setPassword(e.target.value)} minLength={4} required
              className="w-full rounded-xl border border-white/10 bg-black/30 py-3 pl-4 pr-12 font-mono outline-none focus:border-cyan-300/60" />
            <button type="button" onClick={() => setShowPassword(value => !value)}
              aria-label={showPassword ? 'Hide password' : 'Show password'} aria-pressed={showPassword}
              className="absolute inset-y-0 right-0 grid w-12 place-items-center text-slate-400 hover:text-white">
              <svg aria-hidden="true" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" className="size-5">
                {showPassword ? <><path d="m3 3 18 18" /><path d="M10.6 10.6a2 2 0 0 0 2.8 2.8M9.9 4.2A10.5 10.5 0 0 1 12 4c5.5 0 9 8 9 8a18 18 0 0 1-2.1 3.2M6.6 6.6C4.4 8.1 3 12 3 12s3.5 8 9 8a9.8 9.8 0 0 0 4.1-.9" /></> : <><path d="M3 12s3.5-8 9-8 9 8 9 8-3.5 8-9 8-9-8-9-8Z" /><circle cx="12" cy="12" r="3" /></>}
              </svg>
            </button>
          </div>
          <div className="mt-5 flex items-center gap-4">
            <button className="rounded-xl bg-cyan-300 px-5 py-2.5 font-semibold text-slate-950 hover:bg-cyan-200">Save password</button>
            <span className="text-sm text-slate-400">{status}</span>
          </div>
        </form>
        <section className="mt-8 border-t border-white/10 pt-6">
          <h2 className="text-xl font-semibold tracking-tight">Application log</h2>
          <pre className="mt-4 max-h-96 overflow-auto whitespace-pre-wrap rounded-xl bg-black/40 p-4 font-mono text-xs leading-5 text-slate-300">{logs || 'No log output yet.'}</pre>
        </section>
        <div className="mt-8 border-t border-white/10 pt-6">
          <button type="button" onClick={shutdown} disabled={stopping}
            className="rounded-xl border border-red-400/30 px-5 py-2.5 font-semibold text-red-300 hover:bg-red-400/10 disabled:opacity-50">
            {stopping ? 'Stopping…' : 'Stop Beam server'}
          </button>
        </div>
      </section>
    </Shell>
  )
}

function waitForIce(pc: RTCPeerConnection) {
  if (pc.iceGatheringState === 'complete') return Promise.resolve()
  return new Promise<void>(resolve => {
    const listener = () => {
      if (pc.iceGatheringState === 'complete') {
        pc.removeEventListener('icegatheringstatechange', listener)
        resolve()
      }
    }
    pc.addEventListener('icegatheringstatechange', listener)
  })
}

async function measureFrameAge(
  pc: RTCPeerConnection,
  now: number,
  metadata: VideoFrameCallbackMetadata & { captureTime?: number; receiveTime?: number },
) {
  const frameTime = metadata.captureTime ?? metadata.receiveTime
  if (frameTime != null) return now - frameTime

  const stats = await pc.getStats()
  let jitterBufferDelay: number | undefined
  stats.forEach(report => {
    if (report.type === 'inbound-rtp' && report.kind === 'video' && report.jitterBufferDelay != null && report.jitterBufferEmittedCount)
      jitterBufferDelay = report.jitterBufferDelay / report.jitterBufferEmittedCount
  })
  return jitterBufferDelay == null ? null : jitterBufferDelay * 1000
}

function streamPosition(video: HTMLVideoElement, clientX: number, clientY: number) {
  const rect = video.getBoundingClientRect()
  const streamAspect = video.videoWidth && video.videoHeight ? video.videoWidth / video.videoHeight : 16 / 9
  const contentAspect = rect.width / rect.height
  const contentWidth = contentAspect > streamAspect ? rect.height * streamAspect : rect.width
  const contentHeight = contentAspect > streamAspect ? rect.height : rect.width / streamAspect
  const left = rect.left + (rect.width - contentWidth) / 2
  const top = rect.top + (rect.height - contentHeight) / 2
  const x = (clientX - left) / contentWidth
  const y = (clientY - top) / contentHeight
  if (x < 0 || x > 1 || y < 0 || y > 1) return null
  return { x, y }
}

createRoot(document.getElementById('root')!).render(
  <StrictMode>
    <BrowserRouter>
      <Routes>
        <Route path="/" element={<Viewer />} />
        <Route path="/settings" element={<SettingsPage />} />
        <Route path="*" element={<Navigate to="/" replace />} />
      </Routes>
    </BrowserRouter>
  </StrictMode>,
)
