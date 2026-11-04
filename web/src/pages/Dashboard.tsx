import { useEffect, useState } from 'react'
import { Card, Col, Row, Statistic, Table, Tag, Empty, Spin } from 'antd'
import ReactECharts from 'echarts-for-react'
import { api, type StatsQuick, type StatsHeavy, withScanId } from '../api'

function fmtBytes(n: number): string {
  if (!n) return '0 B'
  const units = ['B', 'KB', 'MB', 'GB', 'TB', 'PB']
  const i = Math.floor(Math.log(n) / Math.log(1024))
  return `${(n / Math.pow(1024, i)).toFixed(1)} ${units[i]}`
}

export default function Dashboard() {
  const [quick, setQuick] = useState<StatsQuick | null>(null)
  const [heavy, setHeavy] = useState<StatsHeavy | null>(null)
  const [heavyLoading, setHeavyLoading] = useState(false)

  useEffect(() => {
    // Quick: load immediately (MV-backed, ms)
    api
      .get<StatsQuick>(`/api/stats/quick?${withScanId(new URLSearchParams())}`)
      .then(setQuick)
      .catch(() => setQuick(null))
    // Heavy: load independently (full-scan, ~60s for 22B rows)
    setHeavyLoading(true)
    api
      .get<StatsHeavy>(`/api/stats/heavy?${withScanId(new URLSearchParams())}`)
      .then(setHeavy)
      .catch(() => setHeavy(null))
      .finally(() => setHeavyLoading(false))
  }, [])

  if (!quick) {
    return <Card loading />
  }

  const extPie = {
    tooltip: { trigger: 'item', formatter: (p: { name: string; value: number }) => `${p.name}: ${fmtBytes(p.value)}` },
    series: [
      {
        type: 'pie',
        radius: ['40%', '70%'],
        data: quick.by_extension.map((e) => ({ name: e.extension || '(无扩展名)', value: Number(e.bytes) })),
      },
    ],
  }

  return (
    <div>
      <Row gutter={16} style={{ marginBottom: 16 }}>
        <Col span={8}>
          <Card>
            <Statistic title="当前扫描" value={quick.scan_id || '无数据'} />
          </Card>
        </Col>
        <Col span={8}>
          <Card>
            <Statistic title="硬链接可回收空间" value={fmtBytes(Number(heavy?.hardlink_stats?.wasted_bytes || 0))} valueStyle={{ color: '#fa541c' }} />
          </Card>
        </Col>
        <Col span={8}>
          <Card>
            <Statistic title="断裂软链接" value={Number(quick.symlink_stats?.broken_symlinks || 0)} />
          </Card>
        </Col>
      </Row>

      <Row gutter={16}>
        <Col span={12}>
          <Card title="文件类型分布 (按大小)">
            <ReactECharts option={extPie} style={{ height: 320 }} />
          </Card>
        </Col>
        <Col span={12}>
          <Card title="文件年龄分布 (按修改时间)">
            {heavyLoading ? (
              <div style={{ textAlign: 'center', paddingTop: 120 }}>
                <Spin tip="加载中（22亿数据聚合中...）" />
              </div>
            ) : heavy ? (
              <ReactECharts
                option={{
                  tooltip: { formatter: (p: { name: string; value: number }) => `${p.name}: ${fmtBytes(p.value)}` },
                  xAxis: { type: 'category', data: heavy.by_age.map((a) => a.age) },
                  yAxis: { type: 'value' },
                  series: [{ type: 'bar', data: heavy.by_age.map((a) => Number(a.bytes)), itemStyle: { color: '#1677ff' } }],
                }}
                style={{ height: 320 }}
              />
            ) : (
              <Empty description="加载失败" />
            )}
          </Card>
        </Col>
      </Row>

      <Card title="Top 10 最大目录" style={{ marginTop: 16 }}>
        {heavyLoading ? (
          <div style={{ textAlign: 'center', padding: 60 }}>
            <Spin tip="加载中..." />
          </div>
        ) : heavy ? (
          <ReactECharts
            option={{
              tooltip: { formatter: (p: { name: string; value: number }) => `${p.name}: ${fmtBytes(p.value)}` },
              xAxis: { type: 'value' },
              yAxis: { type: 'category', data: heavy.top_dirs.map((d) => d.dir).reverse() },
              series: [{ type: 'bar', data: heavy.top_dirs.map((d) => Number(d.bytes)).reverse(), itemStyle: { color: '#52c41a' } }],
            }}
            style={{ height: 320 }}
          />
        ) : (
          <Empty />
        )}
      </Card>

      <Card title="文件数最多目录 Top 10" style={{ marginTop: 16 }}>
        {heavyLoading ? (
          <div style={{ textAlign: 'center', padding: 40 }}>
            <Spin tip="加载中..." />
          </div>
        ) : heavy ? (
          <Table
            size="small"
            rowKey="dir"
            pagination={false}
            dataSource={heavy.top_by_filecount || []}
            columns={[
              { title: '目录', dataIndex: 'dir', key: 'dir', ellipsis: true },
              { title: '文件数', dataIndex: 'files', key: 'files', render: (n: number) => Number(n).toLocaleString() },
              { title: '大小', dataIndex: 'bytes', key: 'bytes', render: fmtBytes },
            ]}
          />
        ) : (
          <Empty />
        )}
      </Card>

      {/* Link analysis */}
      <Row gutter={16} style={{ marginTop: 16 }}>
        <Col span={8}>
          <Card>
            <Statistic
              title="硬链接文件数"
              value={Number(heavy?.hardlink_stats?.hardlinked_files || 0)}
            />
            <div style={{ marginTop: 8, color: '#888', fontSize: 12 }}>
              占用 {fmtBytes(Number(heavy?.hardlink_stats?.hardlinked_total_bytes || 0))}
              ，可去重回收{' '}
              <span style={{ color: '#fa541c', fontWeight: 600 }}>
                {fmtBytes(Number(heavy?.hardlink_stats?.wasted_bytes || 0))}
              </span>
            </div>
          </Card>
        </Col>
        <Col span={8}>
          <Card>
            <Statistic
              title="软链接数"
              value={Number(quick.symlink_stats?.total_symlinks || 0)}
            />
            <div style={{ marginTop: 8, color: '#888', fontSize: 12 }}>
              含{' '}
              <span style={{ color: quick.symlink_stats?.broken_symlinks ? '#f5222d' : '#888', fontWeight: 600 }}>
                {Number(quick.symlink_stats?.broken_symlinks || 0)}
              </span>{' '}
              个断裂软链接
            </div>
          </Card>
        </Col>
        <Col span={8}>
          <Card>
            <Statistic title="冷数据 (>1年未访问)" value={fmtBytes(Number(quick.cold_data_bytes))} />
          </Card>
        </Col>
      </Row>

      <Card title="Top 硬链接组（按可回收空间）" style={{ marginTop: 16 }}>
        {heavyLoading ? (
          <div style={{ textAlign: 'center', padding: 40 }}>
            <Spin tip="加载中..." />
          </div>
        ) : heavy && heavy.top_hardlinks && heavy.top_hardlinks.length > 0 ? (
          <Table
            size="small"
            rowKey={(r) => `${r.device}:${r.inode}`}
            pagination={{ pageSize: 5 }}
            dataSource={heavy.top_hardlinks}
            columns={[
              {
                title: 'inode (dev)',
                key: 'inode',
                render: (r) => (
                  <span>
                    {String(r.inode)} <Tag>{'0x' + Number(r.device).toString(16)}</Tag>
                  </span>
                ),
              },
              { title: '链接数', dataIndex: 'link_count', key: 'link_count' },
              { title: '单份大小', dataIndex: 'size_du', key: 'size_du', render: fmtBytes },
              {
                title: '可回收',
                dataIndex: 'wasted_bytes',
                key: 'wasted_bytes',
                render: (n: number) => (
                  <span style={{ color: '#fa541c', fontWeight: 600 }}>{fmtBytes(Number(n))}</span>
                ),
              },
              { title: '路径示例', dataIndex: 'example_path', key: 'example_path', ellipsis: true },
            ]}
          />
        ) : (
          <Empty description={heavyLoading ? '加载中...' : '无硬链接文件'} />
        )}
      </Card>
    </div>
  )
}
