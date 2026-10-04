import { useState } from 'react';
import { Alert, Button, Form, Input, Select, Typography } from 'antd';
import { api, type Agent, upgrading } from './api';

export function connectionLabel(agent: Agent) {
  return agent.connection?.mode === 'controller_to_agent'
    ? 'Controller → Agent'
    : 'Agent → Controller';
}

export type ProxyMode = 'keep' | 'clear' | 'set';
export function proxyValue(mode: ProxyMode, value: string) {
  return mode === 'keep' ? undefined : mode === 'clear' ? null : value;
}
export function ConnectionProxy({
  mode,
  setMode,
  value,
  setValue,
  disabled,
}: {
  mode: ProxyMode;
  setMode: (mode: ProxyMode) => void;
  value: string;
  setValue: (value: string) => void;
  disabled: boolean;
}) {
  return (
    <>
      <Form.Item label="连接代理">
        <Select
          aria-label="连接代理"
          value={mode}
          disabled={disabled}
          onChange={(mode) => {
            setMode(mode);
            setValue('');
          }}
          options={[
            { value: 'keep', label: '保留已有代理（新授权默认直连）' },
            { value: 'clear', label: '不使用代理' },
            { value: 'set', label: '替换代理' },
          ]}
        />
      </Form.Item>
      {mode === 'set' && (
        <Form.Item
          label="SOCKS5 代理地址"
          required
          extra="支持用户名和密码；目标域名由代理解析。保存后不会回显。"
        >
          <Input.Password
            aria-label="SOCKS5 代理地址"
            value={value}
            maxLength={4096}
            autoComplete="new-password"
            visibilityToggle={false}
            disabled={disabled}
            onChange={(e) => setValue(e.target.value)}
            placeholder="socks5://user:password@proxy.example.com:1080"
          />
        </Form.Item>
      )}
    </>
  );
}

export function AgentConnection({ agent, refresh }: { agent: Agent; refresh: () => void }) {
  const [pending, setPending] = useState(false);
  const [error, setError] = useState<string>();
  const [proxyMode, setProxyMode] = useState<ProxyMode>('keep');
  const [proxy, setProxy] = useState('');
  if (agent.connection?.mode !== 'controller_to_agent') return null;
  return (
    <div className="block-gap">
      {agent.connection.last_error && !agent.online && (
        <Alert type="warning" title={agent.connection.last_error} />
      )}
      <Typography.Paragraph>
        保存后立即重新连接，已打开的终端会关闭；应用继续运行。部署或升级期间不能修改。
      </Typography.Paragraph>
      <Typography.Paragraph>
        连接代理：{agent.connection.proxy_configured ? '已配置 SOCKS5' : '直连'}
      </Typography.Paragraph>
      {error && <Alert type="error" title={error} />}
      <Form
        layout="vertical"
        initialValues={{ endpoint: agent.connection.endpoint }}
        onFinish={async ({ endpoint }: { endpoint: string }) => {
          setPending(true);
          setError(undefined);
          try {
            await api(`/v1/agents/${encodeURIComponent(agent.id)}/connection`, 'PUT', {
              endpoint,
              proxy: proxyValue(proxyMode, proxy),
            });
            setProxy('');
            setProxyMode('keep');
            refresh();
          } catch (e) {
            setError((e as Error).message);
          } finally {
            setPending(false);
          }
        }}
      >
        <Form.Item
          name="endpoint"
          label="Agent 可达地址"
          rules={[{ required: true, whitespace: true }]}
        >
          <Input placeholder="agent.example.com:7444" disabled={pending || upgrading(agent)} />
        </Form.Item>
        <ConnectionProxy
          mode={proxyMode}
          setMode={setProxyMode}
          value={proxy}
          setValue={setProxy}
          disabled={pending || upgrading(agent)}
        />
        <Button
          htmlType="submit"
          type="primary"
          aria-label="保存并重连"
          aria-busy={pending}
          loading={pending}
          disabled={pending || upgrading(agent) || (proxyMode === 'set' && !proxy)}
        >
          保存并重连
        </Button>
      </Form>
    </div>
  );
}
