import React, { useEffect, useMemo, useRef, useState } from 'react';
import { createRoot } from 'react-dom/client';
import { invoke } from '@tauri-apps/api/core';
import {
  Ban,
  CheckCircle2,
  CircleAlert,
  Download,
  FolderCog,
  FolderOpen,
  HardDrive,
  Pause,
  Play,
  RefreshCw,
  RotateCcw,
  Search,
  ScrollText,
  Settings,
  Undo2,
  Trash2,
  X,
  XCircle
} from 'lucide-react';
import './styles.css';

type ToastType = 'info' | 'success' | 'warning' | 'error';

interface ToastMessage {
  id: number;
  type: ToastType;
  message: string;
}

type ItemStatus =
  | 'new'
  | 'queued'
  | 'downloading_torrent'
  | 'submitted'
  | 'paused'
  | 'completed'
  | 'deleted'
  | 'ignored'
  | 'failed';

interface AppConfig {
  rssUrl: string;
  rssUrlMasked: string;
  downloadDir: string;
  bitcometExe: string;
  pollIntervalMinutes: number;
  port: number;
  bindHost: string;
  proxyMode: 'no_proxy' | 'system';
}

type DraftConfig = Omit<AppConfig, 'pollIntervalMinutes'> & {
  pollIntervalMinutes: number | '';
};

interface FeedItem {
  id: string;
  title: string;
  link: string;
  guid: string;
  pubDate: string;
  enclosureUrl: string;
  status: ItemStatus;
  downloadDir: string;
  infoHash?: string;
  saveName?: string;
  saveLocation?: string;
  updatedAt: string;
  submittedAt?: string;
  completedAt?: string;
  totalBytes?: number;
  progressPercent?: number;
  lastError?: string;
}

interface RuntimeStatus {
  nextPollAt: string | null;
  polling: boolean;
  lastPollAt: string | null;
  lastPollError: string | null;
  autoDownloadEnabled: boolean;
  dataDir: string;
  bitcometDetectedPath: string;
  bitcometConfigured: boolean;
  bitcometVersion: string | null;
  bitcometRealtime: boolean;
  bitcometRealtimeError: string | null;
  downloadDirReady: boolean;
  startupEnabled: boolean;
  logs: string[];
}

interface BitCometInspection {
  path: string;
  valid: boolean;
  version: string | null;
  supported: boolean;
  error: string | null;
}

const statusText: Record<ItemStatus, string> = {
  new: '新增',
  queued: '排队',
  downloading_torrent: '种子',
  submitted: '下载中',
  paused: '暂停',
  completed: '完成',
  deleted: '删除',
  ignored: '忽略',
  failed: '失败'
};

const downloadDirPlaceholder = String.raw`例如 F:\下载`;
const bitcometExePlaceholder = String.raw`C:\Program Files\BitComet\BitComet_x64.exe`;
const ITEM_REFRESH_INTERVAL_MS = 5000;

function App() {
  const [config, setConfig] = useState<AppConfig | null>(null);
  const [draft, setDraft] = useState<DraftConfig | null>(null);
  const draftDirtyRef = useRef(false);
  const saveTimerRef = useRef<number | null>(null);
  const saveRevisionRef = useRef(0);
  const [items, setItems] = useState<FeedItem[]>([]);
  const [status, setStatus] = useState<RuntimeStatus | null>(null);
  const [bitcometInspection, setBitcometInspection] = useState<BitCometInspection | null>(null);
  const [busy, setBusy] = useState('');
  const [toasts, setToasts] = useState<ToastMessage[]>([]);
  const [tab, setTab] = useState<'items' | 'settings' | 'status'>('items');

  async function loadAll() {
    const [configResponse, itemsResponse, statusResponse] = await Promise.all([
      api<AppConfig>('/api/config'),
      api<{ items: FeedItem[] }>('/api/items'),
      api<RuntimeStatus>('/api/status')
    ]);
    setConfig(configResponse);
    if (!draftDirtyRef.current) {
      setDraft(toDraft(configResponse));
    }
    setItems(itemsResponse.items);
    setStatus(statusResponse);
  }

  useEffect(() => {
    loadAll().catch((error) => showToast(error.message, 'error'));
    const timer = window.setInterval(() => {
      loadAll().catch(() => undefined);
    }, ITEM_REFRESH_INTERVAL_MS);
    return () => {
      window.clearInterval(timer);
      if (saveTimerRef.current !== null) window.clearTimeout(saveTimerRef.current);
    };
  }, []);

  useEffect(() => {
    const path = draft?.bitcometExe.trim() ?? '';
    if (!path) {
      setBitcometInspection(null);
      return;
    }
    let cancelled = false;
    const timer = window.setTimeout(() => {
      api<BitCometInspection>('/api/bitcomet/inspect', {
        method: 'POST',
        body: JSON.stringify({ path })
      }).then((result) => {
        if (!cancelled) setBitcometInspection(result);
      }).catch(() => {
        if (!cancelled) setBitcometInspection(null);
      });
    }, 300);
    return () => {
      cancelled = true;
      window.clearTimeout(timer);
    };
  }, [draft?.bitcometExe]);

  const counts = useMemo(() => {
    return items.reduce<Record<string, number>>((acc, item) => {
      acc[item.status] = (acc[item.status] ?? 0) + 1;
      return acc;
    }, {});
  }, [items]);

  const bitcometPathSaved = Boolean(
    draft?.bitcometExe.trim()
      && config?.bitcometExe.trim()
      && draft.bitcometExe.trim().toLocaleLowerCase() === config.bitcometExe.trim().toLocaleLowerCase()
  );
  const inspectedBitcomet = bitcometInspection?.path.toLocaleLowerCase()
    === (draft?.bitcometExe.trim().toLocaleLowerCase() ?? '')
    ? bitcometInspection
    : null;

  function updateDraft(next: DraftConfig, immediate = false): Promise<void> {
    draftDirtyRef.current = true;
    setDraft(next);
    const revision = ++saveRevisionRef.current;
    if (saveTimerRef.current !== null) window.clearTimeout(saveTimerRef.current);
    if (next.pollIntervalMinutes === '' || next.pollIntervalMinutes < 1 || next.pollIntervalMinutes > 1440) {
      return Promise.resolve();
    }
    const persist = async () => {
      try {
        const saved = await api<AppConfig>('/api/config', {
          method: 'PUT',
          body: JSON.stringify({
            rssUrl: next.rssUrl,
            downloadDir: next.downloadDir,
            bitcometExe: next.bitcometExe,
            pollIntervalMinutes: Number(next.pollIntervalMinutes),
            proxyMode: next.proxyMode
          })
        });
        setConfig(saved);
        if (revision === saveRevisionRef.current) {
          draftDirtyRef.current = false;
          setDraft(toDraft(saved));
        }
        if (immediate) await loadAll();
      } catch (error) {
        if (immediate) showToast((error as Error).message, 'error');
      }
    };
    if (immediate) return persist();
    saveTimerRef.current = window.setTimeout(() => void persist(), 600);
    return Promise.resolve();
  }

  function showToast(message: string, type: ToastType = 'info'): void {
    const id = Date.now() + Math.random();
    setToasts((current) => [...current.slice(-3), { id, type, message }]);
    window.setTimeout(() => dismissToast(id), type === 'error' ? 6000 : 3600);
  }

  function dismissToast(id: number): void {
    setToasts((current) => current.filter((toast) => toast.id !== id));
  }

  async function runAction(label: string, action: () => Promise<void>, options: { reload?: boolean; showBusy?: boolean } = {}) {
    const showBusy = options.showBusy ?? true;
    const reload = options.reload ?? true;
    if (showBusy) setBusy(label);
    try {
      await action();
      if (reload) {
        await loadAll();
      }
    } catch (error) {
      showToast((error as Error).message, 'error');
    } finally {
      if (showBusy) setBusy('');
    }
  }

  async function refreshSubscription() {
    if (!config?.rssUrl.trim()) {
      setTab('settings');
      showToast('尚未配置订阅，请先在配置页填写 RSS 地址。', 'warning');
      return;
    }
    await runAction('refresh', () => api('/api/rss/refresh', { method: 'POST' }).then());
  }

  async function toggleAutoDownload() {
    const enabled = !status?.autoDownloadEnabled;
    await runAction('automation', async () => {
      await api('/api/automation', {
        method: 'PUT',
        body: JSON.stringify({ enabled })
      });
      showToast(enabled ? '自动下载已开启。' : '自动下载已暂停。', 'success');
    });
  }

  async function detectBitComet() {
    await runAction('detect', async () => {
      const result = await api<{ path: string }>('/api/bitcomet/detect');
      if (!result.path) {
        showToast('没有自动找到 BitComet，请手动填写 BitComet.exe 或 BitComet_x64.exe 路径。', 'warning');
        return;
      }
      if (draft) await updateDraft({ ...draft, bitcometExe: result.path }, true);
      showToast(`已找到 BitComet：${result.path}`, 'success');
    }, { reload: false });
  }

  async function selectDownloadDir() {
    if (!draft) return;
    await runAction('select-download-dir', async () => {
      const result = await api<{ path: string }>('/api/dialog/download-dir', {
        method: 'POST',
        body: JSON.stringify({ initialPath: draft.downloadDir })
      });
      if (!result.path) {
        return;
      }
      await updateDraft({ ...draft, downloadDir: result.path }, true);
    }, { reload: false, showBusy: false });
  }

  async function selectBitCometDir() {
    if (!draft) return;
    await runAction('select-bitcomet-dir', async () => {
      const result = await api<{ folder: string; path: string }>('/api/dialog/bitcomet-dir', {
        method: 'POST',
        body: JSON.stringify({ initialPath: draft.bitcometExe })
      });
      if (!result.folder) {
        return;
      }
      if (!result.path) {
        showToast('所选目录中没有找到 BitComet_x64.exe 或 BitComet.exe。', 'warning');
        return;
      }
      await updateDraft({ ...draft, bitcometExe: result.path }, true);
      showToast(`已选择 BitComet：${result.path}`, 'success');
    }, { reload: false, showBusy: false });
  }

  async function deleteTask(item: FeedItem) {
    const confirmed = window.confirm('确认删除该下载任务、已下载文件和缓存的种子文件吗？此操作不可撤销。');
    if (!confirmed) return;
    await runAction(`delete-${item.id}`, () => api(`/api/items/${item.id}/delete`, { method: 'POST' }).then());
  }

  async function controlDownload(item: FeedItem, pause: boolean) {
    const previousStatus = item.status;
    const nextStatus: ItemStatus = pause ? 'paused' : 'submitted';
    const label = `${pause ? 'pause' : 'resume'}-${item.id}`;
    setItems((current) => current.map((entry) => (
      entry.id === item.id ? { ...entry, status: nextStatus } : entry
    )));
    setBusy(label);
    try {
      const result = await api<{ item: FeedItem }>(
        `/api/items/${item.id}/${pause ? 'pause' : 'resume'}`,
        { method: 'POST' }
      );
      setItems((current) => current.map((entry) => (
        entry.id === item.id ? { ...entry, ...result.item } : entry
      )));
      showToast(pause ? '已暂停下载。' : '已继续下载。', 'success');
    } catch (error) {
      setItems((current) => current.map((entry) => (
        entry.id === item.id ? { ...entry, status: previousStatus } : entry
      )));
      showToast((error as Error).message, 'error');
    } finally {
      setBusy('');
    }
  }

  return (
    <main className="app-shell">
      <header className="topbar">
        <div>
          <h1>Mikan下载助手</h1>
          <p>自动追踪 Mikan 订阅，更新后提交到 BitComet</p>
          <small>Designed by CaptainRuby</small>
        </div>
      </header>

      <nav className="tabs" aria-label="页面">
        <button className={tab === 'items' ? 'active' : ''} onClick={() => setTab('items')}>
          <Download size={16} />
          订阅
        </button>
        <button className={tab === 'settings' ? 'active' : ''} onClick={() => setTab('settings')}>
          <Settings size={16} />
          配置
        </button>
        <button className={tab === 'status' ? 'active' : ''} onClick={() => setTab('status')}>
          <ScrollText size={16} />
          日志
        </button>
      </nav>

      <ToastStack toasts={toasts} busy={busy} onDismiss={dismissToast} />

      <section className="summary-grid">
        <Metric icon={<Download size={18} />} label="订阅条目" value={items.length} />
        <Metric icon={<CheckCircle2 size={18} />} label="已完成" value={counts.completed ?? 0} />
        <Metric icon={<Play size={18} />} label="已提交" value={counts.submitted ?? 0} />
        <Metric icon={<XCircle size={18} />} label="失败" value={counts.failed ?? 0} />
      </section>

      {tab === 'items' && (
        <section className="panel subscription-panel">
          <div className="section-heading">
            <h2>订阅列表</h2>
            <div className="section-actions">
              <span>
                {status?.autoDownloadEnabled
                  ? (status.polling ? '正在刷新' : nextPollText(status.nextPollAt))
                  : '自动下载已暂停'}
              </span>
              <button
                className="refresh-button"
                title="同步最新 RSS 条目"
                disabled={Boolean(busy)}
                onClick={refreshSubscription}
              >
                <RefreshCw size={16} />
                刷新订阅
              </button>
              <button
                className={status?.autoDownloadEnabled ? 'danger' : 'success'}
                title={status?.autoDownloadEnabled ? '停止自动轮询和提交' : '开始自动轮询和提交'}
                disabled={Boolean(busy)}
                onClick={toggleAutoDownload}
              >
                {status?.autoDownloadEnabled ? <Pause size={16} /> : <Play size={16} />}
                {status?.autoDownloadEnabled ? '暂停自动下载' : '开启自动下载'}
              </button>
            </div>
          </div>
          <div className="table-wrap">
            <table>
              <thead>
                <tr>
                  <th>标题</th>
                  <th>文件大小</th>
                  <th>状态</th>
                  <th>发布时间</th>
                  <th>目录</th>
                  <th>操作</th>
                </tr>
              </thead>
              <tbody>
                {items.map((item) => (
                  <tr key={item.id}>
                    <td>
                      <div className="title-cell">
                        <strong>{item.title}</strong>
                        {item.lastError && <em>{item.lastError}</em>}
                      </div>
                    </td>
                    <td className="size-cell">{formatFileSize(item.totalBytes)}</td>
                    <td>
                      <div className="status-cell">
                        <StatusPill status={item.status} progress={item.progressPercent} />
                      </div>
                    </td>
                    <td>{formatDate(item.pubDate || item.updatedAt)}</td>
                    <td className="path-cell">
                      {item.saveLocation || item.downloadDir ? (
                        <button
                          className="path-link"
                          title="在文件资源管理器中打开"
                          onClick={() => runAction(
                            `open-${item.id}`,
                            () => api(`/api/items/${item.id}/open-directory`, { method: 'POST' }).then(),
                            { reload: false, showBusy: false }
                          )}
                        >
                          <FolderOpen size={14} />
                          <span>{item.saveLocation || item.downloadDir}</span>
                        </button>
                      ) : '-'}
                    </td>
                    <td>
                      <div className="row-actions">
                        {item.status === 'failed' ? (
                          <button title="重试失败任务" disabled={Boolean(busy)} onClick={() => runAction(`retry-${item.id}`, () => api(`/api/items/${item.id}/retry`, { method: 'POST' }).then())}>
                            <RotateCcw size={15} />
                          </button>
                        ) : item.status === 'submitted' || item.status === 'downloading_torrent' ? (
                          <button title="暂停下载" disabled={Boolean(busy) || !item.infoHash} onClick={() => controlDownload(item, true)}>
                            <Pause size={15} />
                          </button>
                        ) : item.status === 'paused' ? (
                          <button title="继续下载" disabled={Boolean(busy)} onClick={() => controlDownload(item, false)}>
                            <Play size={15} />
                          </button>
                        ) : (
                          <button title="提交下载" disabled={Boolean(busy) || item.status === 'completed' || item.status === 'ignored'} onClick={() => runAction(`download-${item.id}`, () => api(`/api/items/${item.id}/download`, { method: 'POST' }).then())}>
                            <Download size={15} />
                          </button>
                        )}
                        <button
                          title="删除任务和文件"
                          disabled={Boolean(busy) || !item.infoHash || item.status === 'deleted'}
                          onClick={() => deleteTask(item)}
                        >
                          <Trash2 size={15} />
                        </button>
                        {item.status === 'ignored' ? (
                          <button
                            title="取消忽略"
                            disabled={Boolean(busy)}
                            onClick={() => runAction(`unignore-${item.id}`, () => api(`/api/items/${item.id}/unignore`, { method: 'POST' }).then())}
                          >
                            <Undo2 size={15} />
                          </button>
                        ) : (
                          <button
                            title="忽略后续下载"
                            disabled={Boolean(busy) || item.status === 'completed'}
                            onClick={() => runAction(`ignore-${item.id}`, () => api(`/api/items/${item.id}/ignore`, { method: 'POST' }).then())}
                          >
                            <Ban size={15} />
                          </button>
                        )}
                      </div>
                    </td>
                  </tr>
                ))}
                {!items.length && (
                  <tr>
                    <td colSpan={6} className="empty">
                      尚无订阅条目。配置 RSS 地址后点击刷新订阅。
                    </td>
                  </tr>
                )}
              </tbody>
            </table>
          </div>
        </section>
      )}

      {tab === 'settings' && draft && (
        <section className="panel settings-panel">
          <div className="section-heading">
            <h2>服务配置</h2>
          </div>

          <label>
            <span>RSS 地址</span>
            <input
              value={draft.rssUrl}
              placeholder="https://mikanani.me/RSS/MyBangumi?token=..."
              onChange={(event) => updateDraft({ ...draft, rssUrl: event.target.value })}
            />
          </label>

          <label>
            <span>下载目录</span>
            <div className="input-with-button">
              <div className="input-with-icon grow">
                <HardDrive size={16} />
                <input value={draft.downloadDir} placeholder={downloadDirPlaceholder} onChange={(event) => updateDraft({ ...draft, downloadDir: event.target.value })} />
              </div>
              <button onClick={selectDownloadDir} disabled={Boolean(busy)}>
                <FolderOpen size={16} />
                选择
              </button>
            </div>
          </label>

          <label>
            <span>BitComet 路径</span>
            <div className="input-with-button">
              <div className="input-with-icon grow">
                <FolderCog size={16} />
                <input
                  value={draft.bitcometExe}
                  placeholder={bitcometExePlaceholder}
                  onChange={(event) => updateDraft({ ...draft, bitcometExe: event.target.value })}
                />
              </div>
              <button onClick={detectBitComet} disabled={Boolean(busy)}>
                <Search size={16} />
                探测
              </button>
              <button onClick={selectBitCometDir} disabled={Boolean(busy)}>
                <FolderOpen size={16} />
                选择
              </button>
            </div>
          </label>

          <div className={`integration-status ${bitcometPathSaved && status?.bitcometRealtime ? 'ready' : 'warning'}`}>
            <strong>
              {!draft.bitcometExe.trim()
                ? 'BitComet 未配置'
                : inspectedBitcomet && !inspectedBitcomet.valid
                  ? 'BitComet 路径无效'
                  : inspectedBitcomet?.version
                    ? `BitComet v${inspectedBitcomet.version}`
                  : status?.bitcometVersion
                    ? `BitComet v${status.bitcometVersion}`
                    : 'BitComet 版本未知'}
            </strong>
            <span>
              {!draft.bitcometExe.trim()
                ? '请选择 BitComet.exe 或 BitComet_x64.exe。'
                : inspectedBitcomet && !inspectedBitcomet.supported
                  ? inspectedBitcomet.error ?? '当前 BitComet 不符合版本要求。'
                  : inspectedBitcomet?.supported && !bitcometPathSaved
                    ? '路径有效，正在自动保存并连接 WebUI。'
                  : status?.bitcometRealtime
                ? '实时进度连接正常，每 5 秒更新一次。'
                : status?.bitcometRealtimeError ?? '正在检测实时进度接口。'}
            </span>
          </div>

          <div className="form-grid">
            <label>
              <span>代理</span>
              <select
                value={draft.proxyMode}
                onChange={(event) => updateDraft({
                  ...draft,
                  proxyMode: event.target.value as AppConfig['proxyMode']
                }, true)}
              >
                <option value="no_proxy">直连</option>
                <option value="system">使用系统代理</option>
              </select>
            </label>
            <label>
              <span>轮询间隔（分钟）</span>
              <input
                type="number"
                min={1}
                max={1440}
                step={1}
                value={draft.pollIntervalMinutes}
                onChange={(event) => updateDraft({
                  ...draft,
                  pollIntervalMinutes: event.target.value === '' ? '' : Number(event.target.value)
                })}
              />
            </label>
          </div>

        </section>
      )}

      {tab === 'status' && status && (
        <section className="panel log-panel">
          <div className="section-heading">
            <h2>运行日志</h2>
          </div>
          <pre className="logs">{status.logs.length ? status.logs.join('\n') : '暂无日志'}</pre>
        </section>
      )}
      <footer className="footer-credit">Designed by CaptainRuby</footer>
    </main>
  );
}

function toDraft(config: AppConfig): DraftConfig {
  return {
    ...config,
    pollIntervalMinutes: config.pollIntervalMinutes
  };
}

function Metric({ icon, label, value }: { icon: React.ReactNode; label: string; value: number }) {
  return (
    <div className="metric">
      {icon}
      <span>{label}</span>
      <strong>{value}</strong>
    </div>
  );
}

function StatusPill({ status, progress }: { status: ItemStatus; progress?: number }) {
  const showProgress = typeof progress === 'number'
    && (status === 'submitted' || status === 'downloading_torrent');
  const boundedProgress = Math.min(100, Math.max(0, progress ?? 0));
  const label = statusText[status];
  return (
    <span
      className={`status-pill ${status}${showProgress ? ' has-progress' : ''}`}
      style={showProgress ? { '--status-progress': `${boundedProgress}%` } as React.CSSProperties : undefined}
      title={label}
    >
      <span className="status-pill-label">{label}</span>
      {showProgress && (
        <span className="status-pill-fill" aria-hidden="true">
          <span>{label}</span>
        </span>
      )}
    </span>
  );
}

function ToastStack({
  toasts,
  busy,
  onDismiss
}: {
  toasts: ToastMessage[];
  busy: string;
  onDismiss: (id: number) => void;
}) {
  if (!toasts.length && !busy) return null;
  return (
    <div className="toast-stack" aria-live="polite" aria-atomic="true">
      {busy && (
        <div className="toast info">
          <RefreshCw size={17} />
          <span>正在执行：{busyText(busy)}</span>
        </div>
      )}
      {toasts.map((toast) => (
        <div className={`toast ${toast.type}`} key={toast.id}>
          {toast.type === 'success' && <CheckCircle2 size={17} />}
          {toast.type === 'error' && <XCircle size={17} />}
          {toast.type === 'warning' && <CircleAlert size={17} />}
          {toast.type === 'info' && <RefreshCw size={17} />}
          <span>{toast.message}</span>
          <button type="button" aria-label="关闭提示" onClick={() => onDismiss(toast.id)}>
            <X size={15} />
          </button>
        </div>
      ))}
    </div>
  );
}

async function api<T>(url: string, init?: RequestInit): Promise<T> {
  let body: unknown = null;
  if (typeof init?.body === 'string' && init.body) {
    body = JSON.parse(init.body);
  }
  try {
    return await invoke<T>('api_request', {
      path: url,
      method: init?.method ?? 'GET',
      body
    });
  } catch (error) {
    throw new Error(typeof error === 'string' ? error : '本地服务调用失败');
  }
}

function formatDate(value: string): string {
  if (!value) return '-';
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return value;
  return date.toLocaleString();
}

function formatFileSize(bytes?: number): string {
  if (typeof bytes !== 'number' || bytes < 0) return '-';
  if (bytes === 0) return '0 B';
  const units = ['B', 'KB', 'MB', 'GB', 'TB'];
  const unit = Math.min(Math.floor(Math.log(bytes) / Math.log(1024)), units.length - 1);
  const value = bytes / (1024 ** unit);
  return `${new Intl.NumberFormat('zh-CN', { maximumFractionDigits: 2 }).format(value)} ${units[unit]}`;
}

function nextPollText(value?: string | null): string {
  if (!value) return '未计划';
  return `下次轮询 ${formatDate(value)}`;
}

function busyText(value: string): string {
  if (value.startsWith('download-')) return '提交下载';
  if (value.startsWith('retry-')) return '重试任务';
  if (value.startsWith('pause-')) return '暂停下载';
  if (value.startsWith('resume-')) return '继续下载';
  if (value.startsWith('delete-')) return '删除任务和文件';
  if (value.startsWith('unignore-')) return '取消忽略';
  if (value.startsWith('ignore-')) return '忽略任务';
  const labels: Record<string, string> = {
    detect: '探测 BitComet',
    'select-download-dir': '选择下载目录',
    'select-bitcomet-dir': '选择 BitComet 目录',
    'configure-startup': '设置开机自启动',
    startup: '设置开机自启动',
    automation: '更新自动下载',
    refresh: '刷新订阅'
  };
  return labels[value] || value;
}

createRoot(document.getElementById('root')!).render(<App />);
