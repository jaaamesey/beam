import { StrictMode, useEffect, useRef, useState } from 'react'
import { createRoot } from 'react-dom/client'
import { BrowserRouter, Navigate, Route, Routes } from 'react-router'
import './index.css'

type Settings = { password: string; address: string }
const ADMIN_TOKEN = 'beam_admin_token'

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
  const keyboardCleanup = useRef<(() => void) | null>(null)
  const [password, setPassword] = useState('')
  const [status, setStatus] = useState('Ready')
  const [fullscreen, setFullscreen] = useState(false)
  const [streamAspect, setStreamAspect] = useState(16 / 9)

  async function toggleFullscreen() {
    if (document.fullscreenElement) {
      await document.exitFullscreen()
      unlockKeyboard()
    } else {
      const element = player.current
      if (!element) return
      try {
        await (element.requestFullscreen as (options?: { keyboardLock?: 'browser' }) => Promise<void>)({
          keyboardLock: 'browser',
        })
      } catch {
        await element.requestFullscreen()
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
      setFullscreen(document.fullscreenElement === player.current)
      if (!document.fullscreenElement) unlockKeyboard()
    }
    document.addEventListener('fullscreenchange', update)
    return () => document.removeEventListener('fullscreenchange', update)
  }, [])

  function unlockKeyboard() {
    const keyboard = (navigator as Navigator & {
      keyboard?: { unlock?: () => void }
    }).keyboard
    keyboard?.unlock?.()
  }

  async function connect(event: React.FormEvent) {
    event.preventDefault()
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
      const streamHasFocus = () => document.fullscreenElement === player.current || document.activeElement === video.current
      const sendMouseButton = (event: PointerEvent, down: boolean) => {
        if (event.pointerType !== 'mouse' || input.readyState !== 'open') return
        video.current?.focus()
        if (!streamHasFocus()) return
        event.preventDefault()
        if ([0, 1, 2].includes(event.button))
          input.send(JSON.stringify({ type: 'mouseButton', button: event.button, down }))
      }
      const sendKey = (type: 'keyDown' | 'keyUp', event: KeyboardEvent) => {
        if (!streamHasFocus() || event.isComposing || input.readyState !== 'open') return
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
          video.current.onloadedmetadata = () => {
            if (video.current?.videoWidth && video.current.videoHeight)
              setStreamAspect(video.current.videoWidth / video.current.videoHeight)
          }
          video.current.onpointermove = event => {
            if (event.pointerType && event.pointerType !== 'mouse') return
            const position = streamPosition(video.current!, event.clientX, event.clientY)
            if (position && input.readyState === 'open')
              input.send(JSON.stringify({ type: 'mouseMove', ...position }))
          }
          video.current.onwheel = event => {
            event.preventDefault()
            if (input.readyState === 'open')
              input.send(JSON.stringify({
                type: 'wheel', deltaX: event.deltaX, deltaY: event.deltaY, deltaMode: event.deltaMode,
              }))
          }
          video.current.onpointerdown = event => sendMouseButton(event, true)
          video.current.onpointerup = event => sendMouseButton(event, false)
        }
      }
      pc.onconnectionstatechange = () => {
        setStatus(pc.connectionState)
        if (['disconnected', 'failed', 'closed'].includes(pc.connectionState) && video.current)
          video.current.srcObject = null
        if (['disconnected', 'failed', 'closed'].includes(pc.connectionState)) keyboardCleanup.current?.()
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
    keyboardCleanup.current?.()
    peer.current?.close()
  }, [])

  return (
    <Shell>
      <section ref={player} className="relative overflow-hidden rounded-3xl border border-white/10 bg-black shadow-2xl shadow-cyan-950/20">
        <video ref={video} tabIndex={0} controls={false} autoPlay playsInline onClick={() => {
          video.current?.focus()
          void video.current?.play()
        }}
          style={{ aspectRatio: streamAspect }} className="w-full bg-black object-contain" />
        <button type="button" onClick={() => void toggleFullscreen()}
          aria-label={fullscreen ? 'Exit fullscreen' : 'Enter fullscreen'}
          className="absolute right-3 top-3 rounded-lg bg-black/60 px-3 py-2 text-sm font-medium text-white backdrop-blur hover:bg-black/80">
          {fullscreen ? 'Exit fullscreen' : 'Fullscreen'}
        </button>
      </section>
      <form onSubmit={connect} className="mt-6 flex flex-col gap-3 sm:flex-row">
        <input aria-label="Host password" type="password" value={password} onChange={e => setPassword(e.target.value)}
          placeholder="Host password" className="min-w-0 flex-1 rounded-xl border border-white/10 bg-white/5 px-4 py-3 outline-none focus:border-cyan-300/60" />
        <button className="rounded-xl bg-cyan-300 px-6 py-3 font-semibold text-slate-950 hover:bg-cyan-200">Connect</button>
      </form>
      <p className="mt-3 text-sm text-slate-400">{status}</p>
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
        setStatus('Saved on this Mac')
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
