import { useEffect, useMemo, useState, useCallback } from 'react'
import {
  Card,
  Input,
  Table,
  Drawer,
  Descriptions,
  Tag,
  Button,
  Row,
  Col,
  Switch,
  Select,
  Space,
  Tooltip,
} from 'antd'
import {
  FolderOutlined,
  FileOutlined,
  LinkOutlined,
  FilterOutlined,
  AppstoreOutlined,
} from '@ant-design/icons'
import dayjs from 'dayjs'
import { api, type FileEntry, type DirEntry, type SearchFilters, type UidLookupResult, withScanId } from '../api'
import FilterPanel from '../components/FilterPanel'
import DirTree from '../components/DirTree'

function fmtBytes(n: number): string {
  if (!n) return '0 B'
  const units = ['B', 'KB', 'MB', 'GB', 'TB', 'PB']
  const i = Math.floor(Math.log(n) / Math.log(1024))
  return `${(n / Math.pow(1024, i)).toFixed(1)} ${units[i]}`
}

// Resolve uid/gid to names via the backend (which reads /etc/passwd on the
// display node). Cache results so repeated uids only resolve once.
function useUidLookup() {
  const [users, setUsers] = useState<Record<string, string>>({})
  const [groups, setGroups] = useState<Record<string, string>>({})

  const resolve = useCallback((uidSet: Set<number>, gidSet: Set<number>) => {
    const missingUids = [...uidSet].filter((u) => !(String(u) in users))
    const missingGids = [...gidSet].filter((g) => !(String(g) in groups))
    if (missingUids.length === 0 && missingGids.length === 0) return
    const params = new URLSearchParams()
    if (missingUids.length) params.set('uids', missingUids.join(','))
    if (missingGids.length) params.set('gids', missingGids.join(','))
    api
      .get<UidLookupResult>(`/api/uid_lookup?${params}`)
      .then((r) => {
        setUsers((prev) => ({ ...prev, ...r.users }))
        setGroups((prev) => ({ ...prev, ...r.groups }))
      })
      .catch(() => {})
  }, [users, groups])

  return { users, groups, resolve }
}

const AGE_COLORS: Record<string, string> = {
  '0-30d': '#52c41a',
  '30-90d': '#faad14',
  '90-180d': '#fa8c16',
  '180d-1y': '#fa541c',
  '>1y': '#8c8c8c',
}

function ageBucket(mtime: number): string {
  const days = (Date.now() / 1000 - mtime) / 86400
  if (days < 30) return '0-30d'
  if (days < 90) return '30-90d'
  if (days < 180) return '90-180d'
  if (days < 365) return '180d-1y'
  return '>1y'
}

// Heat-based row opacity: newer = full opacity, older = faded.
function heatOpacity(mtime: number): number {
  const days = (Date.now() / 1000 - mtime) / 86400
  if (days < 30) return 1.0
  if (days < 90) return 0.9
  if (days < 180) return 0.75
  if (days < 365) return 0.5
  if (days < 730) return 0.35
  return 0.25
}

const COLUMN_KEYS = ['name', 'size', 'size_du', 'nlink', 'mtime', 'atime', 'age', 'owner', 'extension'] as const
type ColumnKey = (typeof COLUMN_KEYS)[number]
const COLUMN_LABELS: Record<ColumnKey, string> = {
  name: '名称',
  size: '大小(apparent)',
  size_du: '大小(disk)',
  nlink: '硬链接',
  mtime: '修改时间',
  atime: '访问时间',
  age: '热度',
  owner: 'Owner',
  extension: '扩展名',
}

export default function FileBrowser() {
  const [query, setQuery] = useState('')
  const [path, setPath] = useState('')
  const [entries, setEntries] = useState<DirEntry[]>([])
  const [results, setResults] = useState<FileEntry[]>([])
  const [totalMatches, setTotalMatches] = useState<number>(0)
  const [mode, setMode] = useState<'dir' | 'search'>('dir')
  const [detail, setDetail] = useState<FileEntry | null>(null)
  const [loading, setLoading] = useState(false)
  const [filters, setFilters] = useState<SearchFilters>({})
  const [showFilters, setShowFilters] = useState(false)
  const [hiddenCols, setHiddenCols] = useState<Set<ColumnKey>>(new Set(['atime', 'extension', 'size']))
  const [orderBy, setOrderBy] = useState<string>('size_du')
  const [orderDir, setOrderDir] = useState<string>('desc')
  const [useTree, setUseTree] = useState(true)
  const { users, groups, resolve: resolveUids } = useUidLookup()

  // Collect uids/gids from results and resolve them when results change.
  useEffect(() => {
    if (mode !== 'search' || results.length === 0) return
    const uidSet = new Set<number>()
    const gidSet = new Set<number>()
    results.forEach((r) => {
      uidSet.add(r.uid)
      gidSet.add(r.gid)
    })
    resolveUids(uidSet, gidSet)
  }, [results, mode, resolveUids])

  const ownerLabel = (uid: number, gid: number) => {
    const u = users[String(uid)]
    const g = groups[String(gid)]
    return `${u || `uid=${uid}`}:${g || `gid=${gid}`}`
  }

  const activeFilterCount = useMemo(() => {
    let n = 0
    if (filters.min_size) n++
    if (filters.max_size) n++
    if (filters.mtime_after) n++
    if (filters.mtime_before) n++
    if (filters.atime_after) n++
    if (filters.uid) n++
    if (filters.extension) n++
    if (filters.min_nlink) n++
    if (filters.broken_symlink) n++
    if (filters.sparse) n++
    if (filters.kind && filters.kind !== 'all') n++
    return n
  }, [filters])

  // Browse a directory.
  const browse = async (p: string) => {
    setMode('dir')
    setPath(p)
    setLoading(true)
    try {
      const res = await api.get<{ entries: DirEntry[] }>(
        `/api/dir?${withScanId(new URLSearchParams({ path: p }))}`,
      )
      setEntries(res.entries || [])
    } catch {
      setEntries([])
    } finally {
      setLoading(false)
    }
  }

  useEffect(() => {
    browse('')
  }, [])

  // Search across files with filters.
  const search = async (f: SearchFilters) => {
    setMode('search')
    setLoading(true)
    try {
      const params = new URLSearchParams()
      if (f.q) params.set('q', f.q)
      if (f.kind) params.set('kind', f.kind)
      if (f.min_size) params.set('min_size', String(f.min_size))
      if (f.max_size) params.set('max_size', String(f.max_size))
      if (f.mtime_after) params.set('mtime_after', String(f.mtime_after))
      if (f.mtime_before) params.set('mtime_before', String(f.mtime_before))
      if (f.atime_after) params.set('atime_after', String(f.atime_after))
      if (f.uid) params.set('uid', String(f.uid))
      if (f.extension) params.set('extension', f.extension)
      if (f.inode) params.set('inode', String(f.inode))
      if (f.device) params.set('device', String(f.device))
      if (f.min_nlink) params.set('min_nlink', String(f.min_nlink))
      if (f.broken_symlink) params.set('broken_symlink', 'true')
      if (f.sparse) params.set('sparse', 'true')
      params.set('order_by', orderBy)
      params.set('order_dir', orderDir)
      params.set('limit', '200')
      withScanId(params)
      const res = await api.get<{ results: FileEntry[]; total: number }>(`/api/search?${params}`)
      setResults(res.results || [])
      setTotalMatches(Number(res.total) || 0)
    } catch {
      setResults([])
    } finally {
      setLoading(false)
    }
  }

  // Auto-search when filters change (in search mode) or query submitted.
  const doSearch = () => {
    search({ ...filters, q: query })
  }

  // Find every path sharing the same inode (+device) — the hardlink group.
  const searchByInode = (inode: number, device?: number) => {
    setQuery('')
    search({ inode, device, kind: 'all' })
  }

  const onRowClick = (entry: DirEntry) => {
    if (entry.is_dir) {
      const next = path ? `${path}/${entry.name}` : `/${entry.name}`
      browse(next)
    } else {
      const full = path ? `${path}/${entry.name}` : `/${entry.name}`
      api
        .get<FileEntry>(`/api/file?${withScanId(new URLSearchParams({ path: full }))}`)
        .then(setDetail)
        .catch(() => setDetail(null))
    }
  }

  const toggleCol = (k: ColumnKey) => {
    setHiddenCols((prev) => {
      const next = new Set(prev)
      if (next.has(k)) next.delete(k)
      else next.add(k)
      return next
    })
  }

  // Directory browse columns
  const dirColumns = [
    {
      title: '名称',
      dataIndex: 'name',
      key: 'name',
      sorter: (a: DirEntry, b: DirEntry) => a.name.localeCompare(b.name),
      render: (name: string, r: DirEntry) => (
        <a onClick={() => onRowClick(r)}>
          {r.is_dir ? (
            <FolderOutlined style={{ color: '#1677ff' }} />
          ) : r.is_symlink ? (
            <LinkOutlined />
          ) : (
            <FileOutlined />
          )}{' '}
          {name}
          {r.is_symlink ? ' →' : ''}
        </a>
      ),
    },
    { title: '大小(disk)', dataIndex: 'size_du', key: 'size_du', render: fmtBytes },
    { title: '硬链接', dataIndex: 'nlink', key: 'nlink' },
    {
      title: '修改时间',
      dataIndex: 'mtime',
      key: 'mtime',
      render: (t: number) => dayjs.unix(t).format('YYYY-MM-DD HH:mm'),
    },
  ]

  // Search result columns — controlled by hiddenCols + order
  const searchColumns = useMemo(() => {
    const cols: any[] = [
      {
        title: '路径',
        dataIndex: 'path',
        key: 'path',
        render: (p: string, r: FileEntry) => (
          <a onClick={() => setDetail(r)}>
            {r.is_dir ? (
              <FolderOutlined style={{ color: '#1677ff' }} />
            ) : r.is_symlink ? (
              <LinkOutlined />
            ) : (
              <FileOutlined />
            )}{' '}
            {p}
          </a>
        ),
      },
      {
        title: '大小(disk)',
        dataIndex: 'size_du',
        key: 'size_du',
        sorter: true,
        render: fmtBytes,
      },
      {
        title: '大小(apparent)',
        dataIndex: 'size',
        key: 'size',
        render: fmtBytes,
      },
      { title: '硬链接', dataIndex: 'nlink', key: 'nlink', sorter: true },
      {
        title: '修改时间',
        dataIndex: 'mtime',
        key: 'mtime',
        sorter: true,
        render: (t: number) => dayjs.unix(t).format('YYYY-MM-DD HH:mm'),
      },
      {
        title: '访问时间',
        dataIndex: 'atime',
        key: 'atime',
        render: (t: number) => dayjs.unix(t).format('YYYY-MM-DD HH:mm'),
      },
      {
        title: 'Owner',
        key: 'owner',
        render: (_: unknown, r: FileEntry) => ownerLabel(r.uid, r.gid),
      },
      {
        title: '扩展名',
        dataIndex: 'extension',
        key: 'extension',
        render: (e: string) => e || <Tag>无</Tag>,
      },
      {
        title: '热度',
        dataIndex: 'mtime',
        key: 'age',
        render: (t: number) => {
          const b = ageBucket(t)
          return <Tag color={AGE_COLORS[b]}>{b}</Tag>
        },
      },
    ]
    return cols.filter((c) => !hiddenCols.has(c.key))
  }, [hiddenCols])

  const handleTableChange = (pag: any, fil: any, sorter: any) => {
    if (sorter.field) {
      setOrderBy(sorter.field)
      setOrderDir(sorter.order === 'ascend' ? 'asc' : 'desc')
      // re-search with new order
      search({ ...filters, q: query })
    }
  }

  return (
    <div>
      <Card style={{ marginBottom: 16 }}>
        <Space style={{ width: '100%' }} direction="vertical">
          <Space wrap>
            <Input.Search
              placeholder="搜索文件名 / 路径"
              value={query}
              onChange={(e) => setQuery(e.target.value)}
              onSearch={doSearch}
              enterButton="搜索"
              style={{ width: 400 }}
            />
            <Button
              icon={<FilterOutlined />}
              onClick={() => setShowFilters((v) => !v)}
              type={activeFilterCount > 0 ? 'primary' : 'default'}
            >
              过滤 {activeFilterCount > 0 ? `(${activeFilterCount})` : ''}
            </Button>
            {mode === 'search' && (
              <Select
                value={orderBy}
                onChange={(v) => {
                  setOrderBy(v)
                  search({ ...filters, q: query })
                }}
                style={{ width: 130 }}
                options={[
                  { value: 'size_du', label: '按 disk 大小' },
                  { value: 'size', label: '按 apparent' },
                  { value: 'mtime', label: '按修改时间' },
                  { value: 'atime', label: '按访问时间' },
                  { value: 'name', label: '按名称' },
                ]}
              />
            )}
            {mode === 'search' && (
              <Select
                value={orderDir}
                onChange={(v) => {
                  setOrderDir(v)
                  search({ ...filters, q: query })
                }}
                style={{ width: 90 }}
                options={[
                  { value: 'desc', label: '降序' },
                  { value: 'asc', label: '升序' },
                ]}
              />
            )}
            <Tooltip title="列显隐">
              <Select
                mode="multiple"
                placeholder={<AppstoreOutlined />}
                value={COLUMN_KEYS.filter((k) => !hiddenCols.has(k))}
                onChange={(vals: string[]) => {
                  const shown = new Set(vals)
                  setHiddenCols(new Set(COLUMN_KEYS.filter((k) => !shown.has(k))) as Set<ColumnKey>)
                }}
                style={{ width: 200 }}
                maxTagCount={0}
              >
                {COLUMN_KEYS.map((k) => (
                  <Select.Option key={k} value={k}>
                    {COLUMN_LABELS[k]}
                  </Select.Option>
                ))}
              </Select>
            </Tooltip>
            {mode === 'dir' && (
              <Space>
                <Switch checked={useTree} onChange={setUseTree} size="small" />
                <span style={{ fontSize: 12, color: '#888' }}>目录树</span>
              </Space>
            )}
            {mode === 'search' && <Button onClick={() => browse(path)}>返回浏览</Button>}
          </Space>

          {showFilters && <FilterPanel filters={filters} onChange={setFilters} activeCount={activeFilterCount} />}

          {mode === 'dir' && (
            <div>
              <Breadcrumb path={path} onNav={browse} />
            </div>
          )}
        </Space>
      </Card>

      <Row gutter={16}>
        {mode === 'dir' && useTree && (
          <Col span={6}>
            <Card title="目录树" size="small" style={{ height: '100%' }}>
              <DirTree selected={path} onSelect={browse} />
            </Card>
          </Col>
        )}
        <Col span={mode === 'dir' && useTree ? 18 : 24}>
          <Card
            title={
              mode === 'search'
                ? totalMatches > results.length
                  ? `共 ${totalMatches.toLocaleString()} 个,显示前 ${results.length}`
                  : `搜索结果 ${results.length} 个`
                : `目录 ${path || '/'}`
            }
          >
            {mode === 'dir' ? (
              <Table
                columns={dirColumns}
                dataSource={entries}
                rowKey="name"
                loading={loading}
                pagination={{ pageSize: 100, showSizeChanger: true }}
                size="small"
              />
            ) : (
              <Table
                columns={searchColumns}
                dataSource={results}
                rowKey="path"
                loading={loading}
                pagination={{ pageSize: 50, showSizeChanger: true }}
                size="small"
                onChange={handleTableChange}
                rowClassName={(r) => `heat-${ageBucket(r.mtime)}`}
                onRow={(r) => ({
                  style: { opacity: heatOpacity(r.mtime) },
                })}
              />
            )}
          </Card>
        </Col>
      </Row>

      <Drawer open={!!detail} onClose={() => setDetail(null)} title="文件详情" width={480}>
        {detail && (
          <Descriptions column={1} bordered size="small">
            <Descriptions.Item label="路径">{detail.path}</Descriptions.Item>
            <Descriptions.Item label="大小(apparent)">{fmtBytes(detail.size)}</Descriptions.Item>
            <Descriptions.Item label="大小(disk)">{fmtBytes(detail.size_du)}</Descriptions.Item>
            <Descriptions.Item label="Owner">
              {detail ? ownerLabel(detail.uid, detail.gid) : ''}
              <span style={{ color: '#bbb', marginLeft: 8, fontSize: 12 }}>
                ({detail?.uid}:{detail?.gid})
              </span>
            </Descriptions.Item>
            <Descriptions.Item label="扩展名">{detail.extension || '(无)'}</Descriptions.Item>
            <Descriptions.Item label="修改时间">
              {dayjs.unix(detail.mtime).format('YYYY-MM-DD HH:mm:ss')}
            </Descriptions.Item>
            <Descriptions.Item label="访问时间">
              {dayjs.unix(detail.atime).format('YYYY-MM-DD HH:mm:ss')}
            </Descriptions.Item>
            <Descriptions.Item label="Inode (dev)">
              {detail.inode != null ? (
                <span>
                  <a
                    onClick={() => searchByInode(detail.inode!, detail.device)}
                    title="查找同一 inode 的所有路径（硬链接组）"
                  >
                    {String(detail.inode)}
                  </a>{' '}
                  <Tag color="blue">
                    {detail.device != null ? '0x' + Number(detail.device).toString(16) : 'dev?'}
                  </Tag>
                </span>
              ) : (
                <Tag>无 inode（Windows）</Tag>
              )}
            </Descriptions.Item>
            <Descriptions.Item label="硬链接数">{detail.nlink}</Descriptions.Item>
            <Descriptions.Item label="类型">
              {detail.is_dir
                ? '目录'
                : detail.is_symlink
                ? `软链接 → ${detail.symlink_target}`
                : '文件'}
            </Descriptions.Item>
            <Descriptions.Item label="深度">{detail.depth}</Descriptions.Item>
          </Descriptions>
        )}
      </Drawer>
    </div>
  )
}

function Breadcrumb({ path, onNav }: { path: string; onNav: (p: string) => void }) {
  const parts = path.split('/').filter(Boolean)
  return (
    <span style={{ fontSize: 13, color: '#888' }}>
      <a onClick={() => onNav('')}>/</a>
      {parts.map((p, i) => (
        <span key={i}>
          <a onClick={() => onNav(parts.slice(0, i + 1).join('/'))}>{p}</a>
          {i < parts.length - 1 ? '/' : ''}
        </span>
      ))}
    </span>
  )
}
