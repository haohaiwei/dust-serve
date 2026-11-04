import { useEffect, useState } from 'react'
import { Tree, Spin } from 'antd'
import { FolderOutlined } from '@ant-design/icons'
import type { DataNode } from 'antd/es/tree'
import { api, type SubdirEntry, withScanId } from '../api'

interface Props {
  /** Current selected path (empty = root). */
  selected: string
  onSelect: (path: string) => void
}

interface TreeNode extends DataNode {
  key: string
  path: string
  childCount: number
  loaded: boolean
  children?: TreeNode[]
}

export default function DirTree({ selected, onSelect }: Props) {
  const [treeData, setTreeData] = useState<TreeNode[]>([])
  const [loading, setLoading] = useState(false)

  // Load children of a path, return TreeNode[].
  const loadChildren = async (parentPath: string): Promise<TreeNode[]> => {
    const res = await api.get<{ dirs: SubdirEntry[] }>(
      `/api/subdirs?${withScanId(new URLSearchParams({ path: parentPath }))}`,
    )
    return (res.dirs || []).map((d) => {
      const path = parentPath ? `${parentPath}/${d.name}` : `/${d.name}`
      return {
        key: path,
        title: (
          <span>
            <FolderOutlined style={{ color: '#1677ff', marginRight: 4 }} />
            {d.name}
            {d.child_count > 0 ? (
              <span style={{ color: '#999', marginLeft: 4 }}>({d.child_count})</span>
            ) : null}
          </span>
        ),
        path,
        childCount: d.child_count,
        loaded: false,
        isLeaf: d.child_count === 0,
      }
    })
  }

  useEffect(() => {
    setLoading(true)
    loadChildren('')
      .then(setTreeData)
      .catch(() => setTreeData([]))
      .finally(() => setLoading(false))
  }, [])

  // Lazily load children when a node is expanded.
  const onExpand = async (expandedKeys: React.Key[], info: { node: TreeNode }) => {
    const node = info.node
    if (node.loaded || node.childCount === 0) return
    const children = await loadChildren(node.path)
    node.children = children
    node.loaded = true
    // Trigger re-render by replacing treeData.
    setTreeData((prev) => [...prev])
  }

  return (
    <Spin spinning={loading}>
      <Tree
        treeData={treeData}
        selectedKeys={selected ? [selected] : []}
        onSelect={(keys) => {
          const k = keys[0] as string
          onSelect(k || '')
        }}
        onExpand={onExpand}
        blockNode
        showLine
        style={{ maxHeight: 'calc(100vh - 200px)', overflow: 'auto' }}
      />
    </Spin>
  )
}
