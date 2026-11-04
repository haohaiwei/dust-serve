// Thin fetch wrapper that attaches the JWT and talks to /api on the same origin.

const TOKEN_KEY = 'dust_token';

export function getToken(): string | null {
  return localStorage.getItem(TOKEN_KEY);
}

export function setToken(token: string): void {
  localStorage.setItem(TOKEN_KEY, token);
}

export function clearToken(): void {
  localStorage.removeItem(TOKEN_KEY);
}

export class ApiError extends Error {
  status: number;
  constructor(status: number, message: string) {
    super(message);
    this.status = status;
  }
}

// ---- global selected scan_id ----
// When non-null, all data queries (search/dir/file/stats/subdirs/owners/extensions)
// send this scan_id so the UI shows a chosen scan instead of always the latest.
let selectedScanId: string | null = null;
export function getScanId(): string | null {
  return selectedScanId;
}
export function setScanId(id: string | null): void {
  selectedScanId = id;
}
/** Append the current scan_id to a URLSearchParams if one is selected. */
export function withScanId(params: URLSearchParams): URLSearchParams {
  if (selectedScanId) params.set('scan_id', selectedScanId);
  return params;
}

async function request<T>(path: string, init?: RequestInit): Promise<T> {
  const token = getToken();
  const headers: Record<string, string> = {
    'Content-Type': 'application/json',
    ...(init?.headers as Record<string, string>),
  };
  if (token) {
    headers['Authorization'] = `Bearer ${token}`;
  }
  const resp = await fetch(path, { ...init, headers });
  if (resp.status === 401) {
    clearToken();
    window.location.href = '/login';
    throw new ApiError(401, 'unauthorized');
  }
  if (!resp.ok) {
    const text = await resp.text();
    throw new ApiError(resp.status, text || resp.statusText);
  }
  return resp.json() as Promise<T>;
}

export const api = {
  get: <T>(path: string) => request<T>(path),
  post: <T>(path: string, body: unknown) =>
    request<T>(path, { method: 'POST', body: JSON.stringify(body) }),
  del: <T>(path: string) => request<T>(path, { method: 'DELETE' }),
};

// ---- typed responses ----

export interface LoginResponse {
  token: string;
  username: string;
}

export interface ScanRequest {
  root: string;
  scan_id?: string;
  since?: string;
  rate_limit?: number;
  threads?: number;
  batch_size?: number;
  shards?: number;
  /** Overshard factor (default 5). Larger splits deep trees finer. */
  overshard?: number;
  /** Per-scan dirs to exclude (comma-separated names, e.g. "tmp,backup,.git"). */
  exclude_dirs?: string;
  extra_args?: string[];
}

export interface ScanResponse {
  scan_id: string;
  status: string;
}

export interface ScanStatus {
  files: number;
  bytes: number;
  bytes_du: number;
  started: string | null;
  latest: string | null;
}

export interface ScanListItem {
  scan_id: string;
  started: string;
  ended: string;
  files: number;
  dirs: number;
  symlinks: number;
  hardlinks: number;
  total: number;
  bytes_du: number;
}

export interface ScanQps {
  qps: number;
}

export interface ScanShards {
  queue_remaining: number;
  active_pids: number;
}

export interface ScanStates {
  states: Record<string, string>; // scan_id -> "running"|"done"|"failed"|"cancelled"
}

export interface FileEntry {
  path: string;
  name: string;
  extension: string;
  parent_path: string;
  size: number;
  size_du: number;
  mtime: number;
  atime: number;
  ctime: number;
  nlink: number;
  uid: number;
  gid: number;
  inode?: number;
  device?: number;
  is_dir: boolean;
  is_file: boolean;
  is_symlink: boolean;
  symlink_target: string | null;
  depth: number;
}

export interface DirEntry {
  name: string;
  size: number;
  size_du: number;
  mtime: number;
  is_dir: boolean;
  is_file: boolean;
  is_symlink: boolean;
  nlink: number;
  uid: number;
  gid: number;
}

export interface SubdirEntry {
  name: string;
  size_du: number;
  mtime: number;
  child_count: number;
}

export interface OwnerEntry {
  uid: number;
  cnt: number;
  bytes: number;
}

export interface ExtensionEntry {
  extension: string;
  cnt: number;
  bytes: number;
}

export interface UidLookupResult {
  users: Record<string, string>;
  groups: Record<string, string>;
}

export interface SearchFilters {
  q?: string;
  path?: string;
  kind?: string;
  /** Only broken symlinks (target not in scanned paths). */
  broken_symlink?: boolean;
  /** Only sparse files: apparent size > disk usage (size > size_du). */
  sparse?: boolean;
  limit?: number;
  min_size?: number;
  max_size?: number;
  mtime_after?: number;
  mtime_before?: number;
  atime_after?: number;
  atime_before?: number;
  uid?: number;
  extension?: string;
  inode?: number;
  device?: number;
  min_nlink?: number;
  /** UI-only: days for the "近 N 天修改" input (drives mtime_after). */
  _recentDays?: number;
  order_by?: string;
  order_dir?: string;
}

export interface StatsQuick {
  scan_id: string | null;
  by_extension: { extension: string; cnt: number; bytes: number }[];
  cold_data_bytes: string;
  symlink_stats: {
    total_symlinks: number;
    broken_symlinks: number;
  };
}

export interface StatsHeavy {
  by_age: { age: string; bytes: number }[];
  top_dirs: { dir: string; bytes: number; files: number }[];
  top_by_filecount: { dir: string; files: number; bytes: number }[];
  hardlink_stats: {
    hardlinked_files: number;
    wasted_bytes: number;
    hardlinked_total_bytes: number;
  };
  top_hardlinks: {
    device: number;
    inode: number;
    link_count: number;
    size_du: number;
    example_path: string;
    wasted_bytes: number;
  }[];
}

// Legacy combined type (for backward compat, quick + heavy merged).
export interface StatsResponse extends StatsQuick, StatsHeavy {}
