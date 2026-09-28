import { useCallback, useEffect, useRef, useState } from 'react';
import { Alert, Button, Empty, Space, Spin, Table, Tag, Typography } from 'antd';
import { api, ApiError, terminal, type Variable } from './api';

export function useLoad<T>(url: string, interval = 0, untilFinal = false) {
  const [data, setData] = useState<T>();
  const [error, setError] = useState<ApiError>();
  const [revision, setRevision] = useState(0);
  const latest = useRef<T>(undefined);
  const refresh = useCallback(() => setRevision((n) => n + 1), []);
  useEffect(() => {
    setData(undefined);
    setError(undefined);
    latest.current = undefined;
    const controller = new AbortController();
    let pending = false;
    const load = async () => {
      if (pending) return;
      pending = true;
      try {
        const value = await api<T>(url, 'GET', undefined, true, controller.signal);
        if (!controller.signal.aborted) {
          latest.current = value;
          setData(value);
          setError(undefined);
        }
      } catch (e) {
        if (!controller.signal.aborted) setError(e as ApiError);
      } finally {
        pending = false;
      }
    };
    void load();
    const timer = interval
      ? setInterval(() => {
          if (document.visibilityState !== 'visible') return;
          if (untilFinal && terminal.has((latest.current as { state?: string })?.state ?? ''))
            return;
          void load();
        }, interval)
      : undefined;
    return () => {
      controller.abort();
      clearInterval(timer);
    };
  }, [url, interval, revision, untilFinal]);
  return { data, error, refresh };
}
export function ErrorBox({ error, retry }: { error?: Error; retry?: () => void }) {
  return error ? (
    <Alert
      type="error"
      showIcon
      title={error.message}
      action={
        retry && (
          <Button size="small" onClick={retry}>
            重试
          </Button>
        )
      }
    />
  ) : null;
}
export function Loading() {
  return (
    <div className="loading">
      <Spin size="large" />
    </div>
  );
}
export function Heading({
  title,
  subtitle,
  actions,
}: {
  title: string;
  subtitle?: string;
  actions?: React.ReactNode;
}) {
  return (
    <div className="page-heading">
      <div>
        <Typography.Title level={2}>{title}</Typography.Title>
        {subtitle && <Typography.Text type="secondary">{subtitle}</Typography.Text>}
      </div>
      <Space wrap>{actions}</Space>
    </div>
  );
}
const states: Record<string, [string, string]> = {
  building: ['构建中', 'processing'],
  ready: ['准备就绪', 'blue'],
  downloading: ['下载中', 'processing'],
  applying: ['部署中', 'processing'],
  rolling_back: ['回退中', 'warning'],
  succeeded: ['成功', 'success'],
  failed: ['失败', 'error'],
  rolled_back: ['已回退', 'warning'],
  rollback_failed: ['回退失败', 'error'],
  starting: ['启动中', 'processing'],
  running: ['运行中', 'success'],
  backoff: ['等待重启', 'warning'],
  stopped: ['已停止', 'default'],
  authorized: ['已授权', 'blue'],
  issued: ['等待终端确认', 'processing'],
  completed: ['接入完成', 'success'],
  expired: ['已过期', 'warning'],
};
export function StateTag({ state }: { state: string }) {
  const value = states[state] ?? [state, 'default'];
  return <Tag color={value[1]}>{value[0]}</Tag>;
}
export function Variables({ variables }: { variables: Record<string, Variable> }) {
  return (
    <Table
      size="small"
      rowKey="name"
      pagination={false}
      locale={{ emptyText: <Empty description="没有声明变量" /> }}
      dataSource={Object.entries(variables).map(([name, def]) => ({ name, ...def }))}
      columns={[
        { title: '变量', dataIndex: 'name' },
        {
          title: '默认值',
          dataIndex: 'default',
          render: (value: string | null) =>
            value === null ? (
              <Tag color="orange">必填</Tag>
            ) : (
              <Typography.Text code>{JSON.stringify(value)}</Typography.Text>
            ),
        },
      ]}
    />
  );
}
