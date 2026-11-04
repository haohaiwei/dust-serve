import { useEffect, useState } from 'react'
import { Button, Card, Form, Input, Table, Tag, message, Modal } from 'antd'
import { PlayCircleOutlined, ReloadOutlined } from '@ant-design/icons'
import { api, type ScanRequest, type ScanResponse, type ScanListItem, type ScanQps, type ScanShards, setScanId } from '../api'

function fmtBytes(n: number): string {
  if (!n) return '0 B'
  const units = ['B', 'KB', 'MB', 'GB', 'TB', 'PB']
  const i = Math.floor(Math.log(n) / Math.log(1024))
  return `${(n / Math.pow(1024, i)).toFixed(1)} ${units[i]}`
}

const STATUS_META: Record<string, { color: string; text: string }> = {
  running: { color: 'processing', text: '进行中' },
  done: { color: 'success', text: '完成' },
  failed: { color: 'error', text: '失败' },
  cancelled: { color: 'warning', text: '已取消' },
  unknown: { color: 'default', text: '未知' },
}

export default function ScanManager() {
  const [scans, setScans] = useState<ScanListItem[]>([])
  // Real scan states from the server (running/done/failed/cancelled), keyed
  // by scan_id. Falls back to a timestamp guess for scans the server has
  // evicted from memory (e.g. older scans after a restart).
  const [states, setStates] = useState<Record<string, string>>({})
  // Per-scan write rate (rows/sec) for running scans, from query_log.
  const [qpsMap, setQpsMap] = useState<Record<string, number>>({})
  // Per-scan shard status (queue remaining + active pids) for running scans.
  const [shardsMap, setShardsMap] = useState<Record<string, ScanShards>>({})
  // Per-scan calibrating state (button loading).
  const [calibrating, setCalibrating] = useState<Record<string, boolean>>({})
  const [resuming, setResuming] = useState<Record<string, boolean>>({})
  const [loading, setLoading] = useState(false)
  const [starting, setStarting] = useState(false)
  const [form] = Form.useForm()

  const load = async () => {
    setLoading(true)
    try {
      const [listRes, stateRes] = await Promise.all([
        api.get<{ scans: ScanListItem[] }>('/api/scans'),
        api.get<{ states: Record<string, string> }>('/api/scan_states'),
      ])
      setScans(listRes.scans || [])
      setStates(stateRes.states || {})
      // Fetch live QPS for running scans (query_log, ms — doesn't scan file_scan).
      const running = (listRes.scans || []).filter(
        (s) => (stateRes.states || {})[s.scan_id] === 'running',
      )
      const qpsEntries = await Promise.all(
        running.map((s) =>
          api
            .get<ScanQps>(`/api/scan/${encodeURIComponent(s.scan_id)}/qps`)
            .then((r) => [s.scan_id, r.qps] as [string, number])
            .catch(() => [s.scan_id, 0] as [string, number]),
        ),
      )
      setQpsMap(Object.fromEntries(qpsEntries))
      // Fetch shard status (queue remaining + active pids) for running scans.
      const shardsEntries = await Promise.all(
        running.map((s) =>
          api
            .get<ScanShards>(`/api/scan/${encodeURIComponent(s.scan_id)}/shards`)
            .then((r) => [s.scan_id, r] as [string, ScanShards])
            .catch(() => [s.scan_id, { queue_remaining: 0, active_pids: 0 }] as [string, ScanShards]),
        ),
      )
      setShardsMap(Object.fromEntries(shardsEntries))
    } catch (e) {
      message.error((e as Error).message)
    } finally {
      setLoading(false)
    }
  }

  useEffect(() => {
    load()
    const t = setInterval(load, 5000)
    return () => clearInterval(t)
  }, [])

  const startScan = async (values: {
    root: string
    scan_id?: string
    since?: string
    rate_limit?: number
    threads?: number
    shards?: number
    overshard?: number
    exclude_dirs?: string
  }) => {
    setStarting(true)
    try {
      // Form number inputs yield strings; coerce to numbers for the backend
      // (which expects u32/u64, not strings).
      const shardsN = Number(values.shards)
      const overshardN = Number(values.overshard)
      const req: ScanRequest = {
        root: values.root,
        scan_id: values.scan_id || undefined,
        since: values.since || undefined,
        rate_limit: Number(values.rate_limit) || 0,
        threads: values.threads ? Number(values.threads) : undefined,
        shards: shardsN > 1 ? shardsN : undefined,
        overshard: overshardN > 0 ? overshardN : undefined,
        exclude_dirs: values.exclude_dirs || undefined,
      }
      const res = await api.post<ScanResponse>('/api/scan', req)
      message.success(`扫描已启动: ${res.scan_id}`)
      form.resetFields()
      load()
    } catch (e) {
      message.error((e as Error).message)
    } finally {
      setStarting(false)
    }
  }

  const cancelScan = async (scanId: string) => {
    try {
      await api.del<{ status: string }>(`/api/scan/${scanId}`)
      message.success(`已取消: ${scanId}`)
      load()
    } catch (e) {
      message.error((e as Error).message)
    }
  }

  const columns = [
    { title: 'Scan ID', dataIndex: 'scan_id', key: 'scan_id' },
    { title: '开始', dataIndex: 'started', key: 'started' },
    { title: '结束', dataIndex: 'ended', key: 'ended' },
    {
      title: '文件数',
      key: 'files',
      render: (_: unknown, r: ScanListItem) => {
        // files is calibrated (from count() on completion or every 5min while
        // running), so it's accurate even mid-scan. Show it with the live QPS.
        let st = states[r.scan_id]
        if (!st) {
          const recent = r.ended && Date.parse(r.ended) > Date.now() - 60000
          st = recent ? 'running' : 'done'
        }
        if (st === 'running') {
          const q = qpsMap[r.scan_id] || 0
          const f = Number(r.files || 0)
          if (f > 0) {
            return (
              <span style={{ color: '#1677ff' }}>
                {f.toLocaleString()} ({q.toLocaleString()}/s)
              </span>
            )
          }
          return <span style={{ color: '#1677ff' }}>扫描中 ({q.toLocaleString()}/s)</span>
        }
        return Number(r.files || 0).toLocaleString()
      },
    },
    {
      title: '并发',
      key: 'shards',
      render: (_: unknown, r: ScanListItem) => {
        let st = states[r.scan_id]
        if (!st) {
          const recent = r.ended && Date.parse(r.ended) > Date.now() - 60000
          st = recent ? 'running' : 'done'
        }
        if (st !== 'running') return '-'
        const sh = shardsMap[r.scan_id]
        if (!sh) return '-'
        return (
          <span style={{ fontSize: 12, color: '#888' }}>
            {sh.active_pids}进程, 剩{sh.queue_remaining}候选
          </span>
        )
      },
    },
    { title: '大小', dataIndex: 'bytes_du', key: 'bytes_du', render: fmtBytes },
    { title: '目录数', dataIndex: 'dirs', key: 'dirs', render: (n: number) => n ? Number(n).toLocaleString() : '-' },
    { title: '硬链接', dataIndex: 'hardlinks', key: 'hardlinks', render: (n: number) => n ? Number(n).toLocaleString() : '-' },
    { title: '软链接', dataIndex: 'symlinks', key: 'symlinks', render: (n: number) => n ? Number(n).toLocaleString() : '-' },
    {
      title: '状态',
      key: 'status',
      render: (_: unknown, r: ScanListItem) => {
        // Prefer the server's real state; fall back to timestamp guess for
        // scans evicted from memory (older than the server's process).
        let state = states[r.scan_id]
        if (!state) {
          const recent = r.ended && Date.parse(r.ended) > Date.now() - 60000
          state = recent ? 'running' : 'done'
        }
        const meta = STATUS_META[state] || STATUS_META.unknown
        return (
          <span>
            <Tag color={meta.color}>{meta.text}</Tag>
            {state === 'running' && (
              <Button size="small" type="link" onClick={() => cancelScan(r.scan_id)}>
                取消
              </Button>
            )}
            {(state === 'failed' || state === 'cancelled') && (
              <Button
                size="small"
                type="link"
                onClick={() => viewLog(r.scan_id)}
              >
                查日志
              </Button>
            )}
            {(state === 'failed' || state === 'cancelled') && (
              <Button
                size="small"
                type="link"
                loading={!!resuming[r.scan_id]}
                onClick={() => resumeScan(r.scan_id)}
              >
                续扫
              </Button>
            )}
          </span>
        )
      },
    },
    {
      title: '操作',
      key: 'view',
      render: (_: unknown, r: ScanListItem) => (
        <span>
          <Button
            size="small"
            type="link"
            onClick={() => {
              setScanId(r.scan_id)
              window.location.href = '/browse'
            }}
          >
            查看数据
          </Button>
          <Button
            size="small"
            type="link"
            loading={!!calibrating[r.scan_id]}
            onClick={() => calibrateScan(r.scan_id)}
          >
            校准
          </Button>
        </span>
      ),
    },
  ]

  const viewLog = async (scanId: string) => {
    try {
      const log = await api.get<string>(`/api/scan/${scanId}/log`)
      Modal.info({
        title: `扫描日志: ${scanId}`,
        width: 720,
        content: <pre style={{ maxHeight: 400, overflow: 'auto' }}>{log}</pre>,
      })
    } catch (e) {
      message.error((e as Error).message)
    }
  }

  const calibrateScan = async (scanId: string) => {
    setCalibrating((p) => ({ ...p, [scanId]: true }))
    try {
      const res = await api.post<{ files: number; bytes_du: number }>(`/api/scan/${scanId}/calibrate`, {})
      message.success(`校准完成: ${res.files.toLocaleString()} 文件, ${fmtBytes(res.bytes_du)}`)
      load()
    } catch (e) {
      message.error((e as Error).message)
    } finally {
      setCalibrating((p) => ({ ...p, [scanId]: false }))
    }
  }

  // Resume an interrupted scan: the server rebuilds the candidate set from the
  // persisted .meta.json, skips candidates already in .done, and re-runs the
  // rest. Re-scanned (popped-but-incomplete) candidates may write duplicate
  // rows to ClickHouse — accepted, no dedup. "already-complete" means every
  // candidate was in .done, so there's nothing to resume.
  const resumeScan = async (scanId: string) => {
    setResuming((p) => ({ ...p, [scanId]: true }))
    try {
      const res = await api.post<{ scan_id: string; status: string }>(`/api/scan/${scanId}/resume`, {})
      if (res.status === 'already-complete') {
        message.info(`该扫描所有候选已完成,无需续扫`)
      } else {
        message.success(`续扫已启动: ${res.scan_id}`)
      }
      load()
    } catch (e) {
      message.error((e as Error).message)
    } finally {
      setResuming((p) => ({ ...p, [scanId]: false }))
    }
  }

  return (
    <div>
      <Card title="手动触发扫描" style={{ marginBottom: 16 }}>
        <Form form={form} layout="vertical" onFinish={startScan} initialValues={{ rate_limit: 0, shards: 1 }}>
          <Form.Item name="root" rules={[{ required: true }]} label="根目录">
            <Input placeholder="/data" />
          </Form.Item>
          <div style={{ display: 'flex', gap: 12, flexWrap: 'wrap' }}>
            <Form.Item name="scan_id" label="Scan ID（可选）" style={{ flex: 1, minWidth: 200 }}>
              <Input placeholder="自动生成" />
            </Form.Item>
            <Form.Item name="since" label="增量（自日期，可选）" style={{ flex: 1, minWidth: 200 }}>
              <Input placeholder="2025-07-01" />
            </Form.Item>
            <Form.Item name="rate_limit" label="限速(stat/s, 0=不限)" style={{ width: 180 }}>
              <Input type="number" min={0} placeholder="0" />
            </Form.Item>
            <Form.Item name="threads" label="线程数（可选）" style={{ width: 160 }}>
              <Input type="number" min={1} placeholder="默认=CPU核数" />
            </Form.Item>
            <Form.Item name="shards" label="并行进程数" style={{ width: 160 }}>
              <Input type="number" min={1} placeholder="1" />
            </Form.Item>
            <Form.Item name="overshard" label="展开倍数" style={{ width: 160 }}>
              <Input type="number" min={1} placeholder="5" />
            </Form.Item>
          </div>
          <Form.Item name="exclude_dirs" label="排除目录（逗号分隔，如 tmp,backup,.git；.snapshot 默认已排除）">
            <Input placeholder="tmp,backup" />
          </Form.Item>
          <Form.Item>
            <Button type="primary" htmlType="submit" loading={starting} icon={<PlayCircleOutlined />}>
              启动扫描
            </Button>
          </Form.Item>
          <div style={{ color: '#888', fontSize: 12 }}>
            提示：限速用于保护 NFS/Lustre 元数据服务器（建议 50000/s）；线程数大时可加速但增加 MDS 压力；并行进程数&gt;1 时按根目录的子目录拆分多进程并行扫描（子目录不均会失衡）。
          </div>
        </Form>
      </Card>

      <Card
        title="历史扫描"
        extra={<Button icon={<ReloadOutlined />} onClick={load}>刷新</Button>}
      >
        <Table columns={columns} dataSource={scans} rowKey="scan_id" loading={loading} />
      </Card>
    </div>
  )
}
