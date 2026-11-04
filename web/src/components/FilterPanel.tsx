import { useEffect, useState } from 'react'
import { Form, InputNumber, Select, DatePicker, Button, Space, Badge, Tag, Checkbox } from 'antd'
import dayjs from 'dayjs'
import { api, type OwnerEntry, type ExtensionEntry, type SearchFilters, withScanId } from '../api'

interface Props {
  filters: SearchFilters
  onChange: (f: SearchFilters) => void
  activeCount: number
}

const KIND_OPTIONS = [
  { value: 'all', label: '全部' },
  { value: 'file', label: '仅文件' },
  { value: 'dir', label: '仅目录' },
  { value: 'symlink', label: '软链接' },
]

const SIZE_PRESETS = [
  { label: '任意', min: undefined, max: undefined },
  { label: '< 1 MB', min: undefined, max: 1048576 },
  { label: '> 1 MB', min: 1048576, max: undefined },
  { label: '> 100 MB', min: 104857600, max: undefined },
  { label: '> 1 GB', min: 1073741824, max: undefined },
  { label: '> 10 GB', min: 10737418240, max: undefined },
  { label: '> 100 GB', min: 107374182400, max: undefined },
  { label: '> 1 TB', min: 1099511627776, max: undefined },
]

const AGE_PRESETS = [
  { label: '任意', mtimeAfter: undefined, atimeBefore: undefined },
  { label: '近 30 天修改', mtimeAfter: () => Math.floor(Date.now() / 1000) - 30 * 86400, atimeBefore: undefined },
  { label: '近 90 天修改', mtimeAfter: () => Math.floor(Date.now() / 1000) - 90 * 86400, atimeBefore: undefined },
  { label: '近 180 天修改', mtimeAfter: () => Math.floor(Date.now() / 1000) - 180 * 86400, atimeBefore: undefined },
  { label: '近 1 年修改', mtimeAfter: () => Math.floor(Date.now() / 1000) - 365 * 86400, atimeBefore: undefined },
  { label: '> 1 年未访问(冷数据)', mtimeAfter: undefined, atimeBefore: () => Math.floor(Date.now() / 1000) - 365 * 86400 },
]

export default function FilterPanel({ filters, onChange, activeCount }: Props) {
  const [owners, setOwners] = useState<OwnerEntry[]>([])
  const [exts, setExts] = useState<ExtensionEntry[]>([])
  const [sizePreset, setSizePreset] = useState(0)
  const [agePreset, setAgePreset] = useState(0)

  useEffect(() => {
    api.get<{ owners: OwnerEntry[] }>(`/api/owners?${withScanId(new URLSearchParams())}`).then((r) => setOwners(r.owners || [])).catch(() => {})
    api
      .get<{ extensions: ExtensionEntry[] }>(`/api/extensions?${withScanId(new URLSearchParams())}`)
      .then((r) => setExts(r.extensions || []))
      .catch(() => {})
  }, [])

  const update = (patch: Partial<SearchFilters>) => onChange({ ...filters, ...patch })

  const applySizePreset = (idx: number) => {
    setSizePreset(idx)
    const p = SIZE_PRESETS[idx]
    update({ min_size: p.min, max_size: p.max })
  }

  const applyAgePreset = (idx: number) => {
    setAgePreset(idx)
    const p = AGE_PRESETS[idx]
    update({
      mtime_after: typeof p.mtimeAfter === 'function' ? p.mtimeAfter() : undefined,
      atime_before: typeof p.atimeBefore === 'function' ? p.atimeBefore() : undefined,
    })
  }

  const clear = () => {
    setSizePreset(0)
    setAgePreset(0)
    onChange({})
  }

  return (
    <div style={{ padding: '8px 0' }}>
      <Space wrap size="middle">
        <Form.Item label="类型" style={{ marginBottom: 0 }}>
          <Select
            value={filters.kind || 'all'}
            onChange={(v) => update({ kind: v })}
            options={KIND_OPTIONS}
            style={{ width: 100 }}
          />
        </Form.Item>

        <Form.Item label="大小" style={{ marginBottom: 0 }}>
          <Select
            value={sizePreset}
            onChange={applySizePreset}
            options={SIZE_PRESETS.map((p, i) => ({ value: i, label: p.label }))}
            style={{ width: 110 }}
          />
        </Form.Item>

        <Form.Item label="年龄(修改)" style={{ marginBottom: 0 }}>
          <Select
            value={agePreset}
            onChange={applyAgePreset}
            options={AGE_PRESETS.map((p, i) => ({ value: i, label: p.label }))}
            style={{ width: 130 }}
          />
        </Form.Item>

        <Form.Item label="Owner" style={{ marginBottom: 0 }}>
          <Select
            allowClear
            placeholder="任意"
            value={filters.uid}
            onChange={(v) => update({ uid: v })}
            style={{ width: 140 }}
          >
            {owners.map((o) => (
              <Select.Option key={o.uid} value={o.uid}>
                uid={o.uid} ({o.cnt})
              </Select.Option>
            ))}
          </Select>
        </Form.Item>

        <Form.Item label="扩展名" style={{ marginBottom: 0 }}>
          <Select
            allowClear
            showSearch
            placeholder="任意"
            value={filters.extension}
            onChange={(v) => update({ extension: v })}
            style={{ width: 140 }}
          >
            {exts.map((e) => (
              <Select.Option key={e.extension || '(无)'} value={e.extension}>
                {e.extension || '(无)'} ({e.cnt})
              </Select.Option>
            ))}
          </Select>
        </Form.Item>

        <Form.Item label="自定义大小" style={{ marginBottom: 0 }}>
          <Space>
            <InputNumber
              placeholder="min B"
              value={filters.min_size}
              onChange={(v) => update({ min_size: v ?? undefined })}
              style={{ width: 110 }}
            />
            <InputNumber
              placeholder="max B"
              value={filters.max_size}
              onChange={(v) => update({ max_size: v ?? undefined })}
              style={{ width: 110 }}
            />
          </Space>
        </Form.Item>

        <Form.Item label="修改时间区间" style={{ marginBottom: 0 }}>
          <DatePicker.RangePicker
            showTime
            onChange={(_, [s, e]) => {
              const after = s ? Math.floor(dayjs(s).unix()) : undefined
              const before = e ? Math.floor(dayjs(e).unix()) : undefined
              update({ mtime_after: after, mtime_before: before })
            }}
            style={{ width: 360 }}
          />
        </Form.Item>

        <Form.Item label="硬链接数 ≥" style={{ marginBottom: 0 }}>
          <InputNumber
            min={1}
            placeholder="如 2"
            value={filters.min_nlink}
            onChange={(v) => update({ min_nlink: v ?? undefined })}
            style={{ width: 110 }}
          />
        </Form.Item>

        <Form.Item label="近 N 天修改" style={{ marginBottom: 0 }}>
          <InputNumber
            min={1}
            placeholder="如 7"
            value={filters._recentDays}
            onChange={(v) => {
              const days = v ?? undefined
              const after = days ? Math.floor(Date.now() / 1000) - days * 86400 : undefined
              update({ _recentDays: days, mtime_after: after, mtime_before: undefined })
            }}
            style={{ width: 110 }}
            addonAfter="天"
          />
        </Form.Item>

        <Form.Item style={{ marginBottom: 0 }}>
          <Checkbox
            checked={!!filters.broken_symlink}
            onChange={(e) => update({ broken_symlink: e.target.checked })}
          >
            仅断裂软链接
          </Checkbox>
        </Form.Item>

        <Form.Item style={{ marginBottom: 0 }}>
          <Checkbox
            checked={!!filters.sparse}
            onChange={(e) => update({ sparse: e.target.checked })}
          >
            仅稀疏文件
          </Checkbox>
        </Form.Item>

        <Button onClick={clear}>清除</Button>
        {activeCount > 0 && <Badge count={activeCount} style={{ backgroundColor: '#1677ff' }} />}
      </Space>
    </div>
  )
}
