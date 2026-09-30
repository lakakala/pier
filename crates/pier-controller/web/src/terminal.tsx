import { useEffect, useRef, useState } from 'react';
import { Alert, Button, Drawer, Space, Tag, Tooltip } from 'antd';
import { Terminal } from '@xterm/xterm';
import { FitAddon } from '@xterm/addon-fit';
import '@xterm/xterm/css/xterm.css';
import { api, type Agent, upgrading } from './api';

const reasons: Record<string, string> = {
  shell_exited: 'Bash 已退出',
  deployment_started: '开始重新部署，终端已关闭',
  agent_upgrading: 'Agent 正在升级，终端已关闭',
  agent_stopping: 'Agent 已停止',
  terminal_unavailable: '无法打开终端：应用未安装、正在部署，或账户不可用',
  session_revoked_or_agent_disconnected: '登录失效或服务器连接已断开',
  login_expired: '登录已过期',
  connection_lost: '服务器连接已断开',
  connection_closed: '终端连接已关闭',
};

export function AppTerminal({
  agent,
  instance,
  name,
}: {
  agent: Agent;
  instance: string;
  name: string;
}) {
  const [open, setOpen] = useState(false);
  const [full, setFull] = useState(false);
  const unavailable = !agent.online
    ? '服务器离线'
    : upgrading(agent)
      ? 'Agent 正在升级'
      : !agent.report.capabilities?.includes('app_terminal_v1')
        ? 'Agent 升级后可使用终端'
        : '';
  return (
    <>
      <Tooltip title={unavailable}>
        <span>
          <Button disabled={!!unavailable} onClick={() => setOpen(true)}>
            终端
          </Button>
        </span>
      </Tooltip>
      <Drawer
        title={`${agent.name || agent.id} / ${name} · Bash`}
        open={open}
        onClose={() => setOpen(false)}
        width={full ? '100%' : 960}
        destroyOnHidden
        extra={<Button onClick={() => setFull(!full)}>{full ? '退出全屏' : '全屏'}</Button>}
      >
        {open && <TerminalSession agent={agent.id} instance={instance} />}
      </Drawer>
    </>
  );
}

function TerminalSession({ agent, instance }: { agent: string; instance: string }) {
  const element = useRef<HTMLDivElement>(null);
  const [attempt, setAttempt] = useState(0);
  const [connected, setConnected] = useState(false);
  const [ended, setEnded] = useState(false);
  const [identity, setIdentity] = useState('正在连接…');
  const [error, setError] = useState('');
  useEffect(() => {
    const terminal = new Terminal({
      cursorBlink: true,
      scrollback: 2000,
      fontSize: 14,
      theme: { background: '#101828', foreground: '#f2f4f7' },
      linkHandler: { activate: () => {} },
    });
    const fit = new FitAddon();
    terminal.loadAddon(fit);
    terminal.open(element.current!);
    // Do not allow application output to write to the system clipboard.
    terminal.parser.registerOscHandler(52, () => true);
    let socket: WebSocket | undefined;
    let disposed = false;
    let ready = false;
    let reportedExit = false;
    let resizeFrame = 0;
    const abort = new AbortController();
    setConnected(false);
    setEnded(false);
    setIdentity('正在连接…');
    setError('');
    const control = (value: unknown) => {
      if (socket?.readyState === WebSocket.OPEN) socket.send(JSON.stringify(value));
    };
    const fitGrid = () => {
      fit.fit();
      terminal.resize(Math.min(500, terminal.cols), Math.min(500, terminal.rows));
    };
    const resize = () => {
      cancelAnimationFrame(resizeFrame);
      resizeFrame = requestAnimationFrame(() => {
        if (disposed || !element.current?.clientWidth) return;
        fitGrid();
        if (ready)
          control({
            type: 'resize',
            cols: Math.min(500, terminal.cols),
            rows: Math.min(500, terminal.rows),
          });
      });
    };
    const observer = new ResizeObserver(resize);
    observer.observe(element.current!);
    const sendInput = (data: string, binary = false) => {
      if (!ready || socket?.readyState !== WebSocket.OPEN) return;
      if (data.length > 256 * 1024) {
        setError('输入过长，请分批粘贴');
        return;
      }
      const bytes = binary
        ? Uint8Array.from(data, (character) => character.charCodeAt(0) & 255)
        : new TextEncoder().encode(data);
      if (socket.bufferedAmount + bytes.length > 256 * 1024) {
        setError('输入发送中，请稍后重试或分批粘贴');
        return;
      }
      for (let offset = 0; offset < bytes.length; offset += 32768)
        socket.send(bytes.slice(offset, offset + 32768));
    };
    const input = terminal.onData((data) => sendInput(data));
    const binaryInput = terminal.onBinary((data) => sendInput(data, true));
    const start = requestAnimationFrame(() => {
      fitGrid();
      void api<{ websocket_url: string }>(
        `/v1/agents/${agent}/apps/${instance}/terminals`,
        'POST',
        { cols: Math.min(500, terminal.cols), rows: Math.min(500, terminal.rows) },
        true,
        abort.signal,
      )
        .then((ticket) => {
          if (disposed) return;
          const url = new URL(ticket.websocket_url, window.location.href);
          url.protocol = location.protocol === 'https:' ? 'wss:' : 'ws:';
          socket = new WebSocket(url);
          socket.binaryType = 'arraybuffer';
          socket.onmessage = (event: MessageEvent<string | ArrayBuffer>) => {
            if (disposed) return;
            if (event.data instanceof ArrayBuffer) {
              const bytes = new Uint8Array(event.data);
              terminal.write(bytes, () => control({ type: 'ack', bytes: bytes.length }));
              return;
            }
            try {
              const message = JSON.parse(event.data);
              if (message.type === 'ready') {
                ready = true;
                setConnected(true);
                setIdentity(`${message.user} · ${message.home}`);
                terminal.focus();
                resize();
              } else if (message.type === 'exit') {
                ready = false;
                reportedExit = true;
                setConnected(false);
                setEnded(true);
                setError(reasons[message.reason] || '终端已关闭');
              }
            } catch {
              setError('终端响应无效');
              socket?.close();
            }
          };
          socket.onclose = () => {
            if (disposed) return;
            ready = false;
            setConnected(false);
            setEnded(true);
            if (!reportedExit) setError('连接已断开，重新连接将创建新的 Bash 会话');
          };
          socket.onerror = () => {
            if (!disposed) setError('连接失败，请检查服务器状态和 WebSocket 代理配置');
          };
        })
        .catch((error: Error) => {
          if (!disposed) {
            setError(error.message);
            setEnded(true);
          }
        });
    });
    return () => {
      disposed = true;
      ready = false;
      abort.abort();
      cancelAnimationFrame(start);
      cancelAnimationFrame(resizeFrame);
      observer.disconnect();
      input.dispose();
      binaryInput.dispose();
      socket?.close();
      terminal.dispose();
    };
  }, [agent, instance, attempt]);
  return (
    <>
      <Space className="block-gap">
        <Tag color={connected ? 'green' : 'default'}>
          {connected ? '已连接' : ended ? '已断开' : '连接中'}
        </Tag>
        <span>{identity}</span>
        <Button disabled={!ended} onClick={() => setAttempt((n) => n + 1)}>
          重新连接
        </Button>
      </Space>
      {error && <Alert className="block-gap" type={ended ? 'info' : 'warning'} title={error} />}
      <div ref={element} className="app-terminal" aria-label="应用 Bash 终端" />
      <p>关闭终端会结束 Bash 会话；应用自动重启不会中断此终端。</p>
    </>
  );
}
