import { useEffect, useState } from 'react'
import { Navigate, Route, Routes, useLocation } from 'react-router-dom'
import { Layout, Menu, Select, Button } from 'antd'
import { DesktopOutlined, FolderOpenOutlined, ScanOutlined } from '@ant-design/icons'
import { api, getToken, getScanId, setScanId, type ScanListItem } from './api'
import Login from './pages/Login'
import ScanManager from './pages/ScanManager'
import FileBrowser from './pages/FileBrowser'
import Dashboard from './pages/Dashboard'

const { Header, Sider, Content } = Layout

function Shell() {
  const [scans, setScans] = useState<ScanListItem[]>([])
  const [curScan, setCurScan] = useState<string>(getScanId() || '')
  const [tick, setTick] = useState(0) // bump to remount pages after switching scan
  const loc = useLocation()

  useEffect(() => {
    // Load available scans for the selector. Best-effort.
    api
      .get<{ scans: ScanListItem[] }>('/api/scans')
      .then((r) => setScans(r.scans || []))
      .catch(() => {})
  }, [])

  const onChangeScan = (sid: string) => {
    setCurScan(sid)
    setScanId(sid || null)
    setTick((t) => t + 1) // remount pages so they reload with the new scan_id
  }

  return (
    <Layout style={{ minHeight: '100vh' }}>
      <Sider collapsible>
        <div style={{ height: 32, margin: 16, color: '#fff', fontSize: 18, textAlign: 'center' }}>
          dust
        </div>
        <Menu theme="dark" defaultSelectedKeys={['scan']} mode="inline" selectedKeys={[loc.pathname.replace('/', '')]}>
          <Menu.Item key="scan" icon={<ScanOutlined />}>
            <a href="/scan">扫描管理</a>
          </Menu.Item>
          <Menu.Item key="browse" icon={<FolderOpenOutlined />}>
            <a href="/browse">文件浏览</a>
          </Menu.Item>
          <Menu.Item key="dashboard" icon={<DesktopOutlined />}>
            <a href="/dashboard">仪表盘</a>
          </Menu.Item>
        </Menu>
      </Sider>
      <Layout>
        <Header style={{ background: '#fff', padding: '0 16px', display: 'flex', alignItems: 'center', gap: 16 }}>
          <span style={{ fontSize: 16, fontWeight: 500 }}>文件系统分析平台</span>
          <div style={{ marginLeft: 'auto', display: 'flex', alignItems: 'center', gap: 8 }}>
            <span style={{ color: '#888', fontSize: 13 }}>当前扫描:</span>
            <Select
              style={{ width: 260 }}
              showSearch
              placeholder="默认(最新扫描)"
              value={curScan || undefined}
              onChange={onChangeScan}
              allowClear
              options={scans.map((s) => ({
                value: s.scan_id,
                label: `${s.scan_id} (${Number(s.files || 0).toLocaleString()} 文件)`,
              }))}
            />
            {curScan && (
              <Button size="small" onClick={() => onChangeScan('')}>
                清除(用最新)
              </Button>
            )}
          </div>
        </Header>
        <Content style={{ margin: 16, background: '#fff', padding: 24, borderRadius: 8 }}>
          <Routes key={tick}>
            <Route path="/scan" element={<ScanManager />} />
            <Route path="/browse" element={<FileBrowser />} />
            <Route path="/dashboard" element={<Dashboard />} />
            <Route path="*" element={<Navigate to="/scan" replace />} />
          </Routes>
        </Content>
      </Layout>
    </Layout>
  )
}

export default function App() {
  if (!getToken()) {
    return (
      <Routes>
        <Route path="/login" element={<Login />} />
        <Route path="*" element={<Navigate to="/login" replace />} />
      </Routes>
    )
  }
  return (
    <Routes>
      <Route path="/login" element={<Navigate to="/scan" replace />} />
      <Route path="/*" element={<Shell />} />
    </Routes>
  )
}
