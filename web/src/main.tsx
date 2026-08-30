import { StrictMode, useEffect, useRef, useState } from 'react'
import { createRoot } from 'react-dom/client'
import { BrowserRouter, Navigate, Route, Routes } from 'react-router'
import './index.css'

type Settings = { password: string }

async function json<T>(path: string, init?: RequestInit): Promise<T> {
  const response = await fetch(path, {
    credentials: 'same-origin',
    headers: { 'content-type': 'application/json', ...init?.headers },
    ...init,
  })
  if (!response.ok) throw new Error((await response.text()) || response.statusText)
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
  const peer = useRef<RTCPeerConnection | null>(null)
  const [password, setPassword] = useState('')
  const [status, setStatus] = useState('Ready')

  async function connect(event: React.FormEvent) {
    event.preventDefault()
    setStatus('Authenticating…')
    try {
      await json('/api/session', { method: 'POST', body: JSON.stringify({ password }) })
      const pc = new RTCPeerConnection({ iceServers: [] })
      peer.current?.close()
      peer.current = pc
      pc.addTransceiver('video', { direction: 'recvonly' })
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
        if (video.current) video.current.srcObject = stream
      }
      pc.onconnectionstatechange = () => {
        setStatus(pc.connectionState)
        if (['disconnected', 'failed', 'closed'].includes(pc.connectionState) && video.current)
          video.current.srcObject = null
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

  useEffect(() => () => peer.current?.close(), [])

  return (
    <Shell>
      <section className="overflow-hidden rounded-3xl border border-white/10 bg-black shadow-2xl shadow-cyan-950/20">
        <video ref={video} autoPlay playsInline className="aspect-video w-full bg-black object-contain" />
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
  const [password, setPassword] = useState('')
  const [status, setStatus] = useState('Loading…')

  useEffect(() => {
    json<Settings>('/api/admin/settings')
      .then(value => { setPassword(value.password); setStatus('Saved on this Mac') })
      .catch(error => setStatus(error instanceof Error ? error.message : 'Unavailable'))
  }, [])

  async function save(event: React.FormEvent) {
    event.preventDefault()
    setStatus('Saving…')
    try {
      await json('/api/admin/settings', { method: 'PUT', body: JSON.stringify({ password }) })
      setStatus('Saved')
    } catch (error) {
      setStatus(error instanceof Error ? error.message : 'Save failed')
    }
  }

  return (
    <Shell>
      <section className="max-w-xl rounded-3xl border border-white/10 bg-white/[.04] p-7 shadow-2xl">
        <p className="mb-2 text-xs font-semibold uppercase tracking-[.2em] text-cyan-300">Host settings</p>
        <h1 className="text-3xl font-semibold tracking-tight">Who can connect?</h1>
        <p className="mt-3 text-sm leading-6 text-slate-400">This page can change settings only when opened on the host Mac.</p>
        <form onSubmit={save} className="mt-8">
          <label className="mb-2 block text-sm font-medium" htmlFor="password">Client password</label>
          <input id="password" value={password} onChange={e => setPassword(e.target.value)} minLength={12} required
            className="w-full rounded-xl border border-white/10 bg-black/30 px-4 py-3 font-mono outline-none focus:border-cyan-300/60" />
          <div className="mt-5 flex items-center gap-4">
            <button className="rounded-xl bg-cyan-300 px-5 py-2.5 font-semibold text-slate-950 hover:bg-cyan-200">Save password</button>
            <span className="text-sm text-slate-400">{status}</span>
          </div>
        </form>
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
