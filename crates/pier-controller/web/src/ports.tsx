import { useState } from 'react';
import { Link } from 'react-router-dom';
import {
  App,
  AutoComplete,
  Button,
  Card,
  Checkbox,
  Form,
  InputNumber,
  Modal,
  Select,
  Space,
  Table,
  Tag,
  Typography,
} from 'antd';
import { api, type Agent, type PortDefinition, type ExposureMap } from './api';
import { ErrorBox, useLoad } from './components';

export function AppPorts({ ports = {} }: { ports?: Record<string, PortDefinition> }) {
  return (
    <Table
      size="small"
      rowKey="name"
      pagination={false}
      dataSource={Object.entries(ports).map(([name, port]) => ({ name, ...port }))}
      columns={[
        { title: '端口名', dataIndex: 'name' },
        { title: '协议', dataIndex: 'protocol', render: (value) => (value ?? 'tcp').toUpperCase() },
        { title: '应用端口', dataIndex: 'port', render: (value) => <code>{value}</code> },
      ]}
    />
  );
}
export function AgentTags({ agent, refresh }: { agent: Agent; refresh: () => void }) {
  const [open, setOpen] = useState(false);
  const [tags, setTags] = useState<string[]>([]);
  const [pending, setPending] = useState(false);
  const [error, setError] = useState<Error>();
  const { message } = App.useApp();
  return (
    <div className="block-gap">
      <Space wrap>
        <Typography.Text>标签</Typography.Text>
        {(agent.tags ?? []).map((tag) => (
          <Tag key={tag}>{tag}</Tag>
        ))}
        <Button
          onClick={() => {
            setTags(agent.tags ?? []);
            setError(undefined);
            setOpen(true);
          }}
        >
          编辑标签
        </Button>
      </Space>
      <Modal
        title="编辑 agent 标签"
        open={open}
        onCancel={() => setOpen(false)}
        confirmLoading={pending}
        onOk={async () => {
          setPending(true);
          setError(undefined);
          try {
            await api(`/v1/agents/${agent.id}/tags`, 'PUT', { tags });
            refresh();
            setOpen(false);
            void message.success('标签已保存，已部署应用的入口将自动调整');
          } catch (e) {
            setError(e as Error);
          } finally {
            setPending(false);
          }
        }}
      >
        <ErrorBox error={error} />
        <Typography.Paragraph>
          所有匹配标签的服务器都会提供入口。修改标签立即作用于已部署的端口配置。
        </Typography.Paragraph>
        <Select
          aria-label="Agent 标签"
          mode="tags"
          style={{ width: '100%' }}
          value={tags}
          onChange={setTags}
          tokenSeparators={[',']}
        />
      </Modal>
    </div>
  );
}
export type BindingPort = { app: string; name: string; definition: PortDefinition };
export function initialPorts(ports: BindingPort[], exposures?: ExposureMap) {
  return ports.map(({ app, name, definition }) => ({
    enabled: !!exposures?.[app]?.[name],
    tag: exposures?.[app]?.[name]?.tag,
    port:
      exposures?.[app]?.[name]?.port ??
      (typeof definition.port === 'number' ? definition.port : undefined),
  }));
}
export function BindingPorts({ ports, tags }: { ports: BindingPort[]; tags: string[] }) {
  if (!ports.length) return null;
  return (
    <Card size="small" title="端口暴露（下次部署成功后生效）" className="block-gap">
      <Typography.Paragraph type="secondary">
        选择入口标签和对外端口，所有匹配服务器都会提供入口。
      </Typography.Paragraph>
      {ports.map(({ app, name, definition }, index) => (
        <div key={`${app}/${name}`} className="variable-row">
          <Form.Item name={['ports', index, 'enabled']} valuePropName="checked">
            <Checkbox>
              {app} / {name} · {(definition.protocol ?? 'tcp').toUpperCase()} · 应用端口{' '}
              {definition.port}
            </Checkbox>
          </Form.Item>
          <Form.Item
            noStyle
            shouldUpdate={(previous, current) =>
              previous.ports?.[index]?.enabled !== current.ports?.[index]?.enabled
            }
          >
            {({ getFieldValue }) =>
              getFieldValue(['ports', index, 'enabled']) && (
                <Space align="start" wrap>
                  <Form.Item
                    name={['ports', index, 'tag']}
                    label={`${app}/${name} 入口标签`}
                    rules={[{ required: true, whitespace: true, message: '请输入入口标签' }]}
                  >
                    <AutoComplete
                      options={tags.map((value) => ({ value }))}
                      style={{ minWidth: 180 }}
                      placeholder="选择或输入标签"
                    />
                  </Form.Item>
                  <Form.Item
                    name={['ports', index, 'port']}
                    label={`${app}/${name} 对外端口`}
                    rules={[
                      {
                        required: true,
                        type: 'integer',
                        min: 1,
                        max: 65535,
                        message: '请输入 1–65535 的端口',
                      },
                    ]}
                  >
                    <InputNumber min={1} max={65535} precision={0} />
                  </Form.Item>
                </Space>
              )
            }
          </Form.Item>
        </div>
      ))}
    </Card>
  );
}
interface ExposureStatus {
  id: string;
  agent_id: string;
  blueprint: string;
  deployment: string;
  app: string;
  name: string;
  tag: string;
  protocol: 'tcp' | 'udp';
  port: number;
  target_port: number;
  ingress_id: string | null;
  ingress_name: string | null;
  state: 'pending' | 'ready' | 'error';
  reason: string | null;
}
export function Exposures({ id }: { id: string }) {
  const status = useLoad<{ exposures: ExposureStatus[] }>(`/v1/agents/${id}/exposures`, 3000);
  const rows = status.data?.exposures ?? [];
  const ready = rows.filter((row) => row.state === 'ready').length;
  const summary = !rows.length
    ? '暂无已部署端口'
    : ready === rows.length
      ? '可用'
      : ready
        ? '部分可用'
        : rows.some((row) => row.state === 'error')
          ? '失败'
          : '等待中';
  return (
    <Card
      title="当前生效的端口暴露"
      className="block-gap"
      extra={
        <Tag color={ready === rows.length && ready ? 'success' : ready ? 'warning' : 'default'}>
          {summary}
        </Tag>
      }
    >
      <ErrorBox error={status.error} retry={status.refresh} />
      <Typography.Paragraph type="secondary">
        入口状态独立于应用部署结果；标签变更和重新连接会自动调整入口。
      </Typography.Paragraph>
      <Table
        rowKey="id"
        dataSource={rows}
        pagination={false}
        scroll={{ x: 800 }}
        columns={[
          {
            title: '应用',
            render: (_, row) => (
              <Space orientation="vertical" size={0}>
                <Link to={`/agents/${row.agent_id}`}>
                  {row.blueprint} / {row.app}
                </Link>
                <Link to={`/deployments/${row.deployment}`}>当前部署</Link>
              </Space>
            ),
          },
          {
            title: '命名端口',
            render: (_, row) => `${row.name} · ${row.protocol.toUpperCase()} :${row.target_port}`,
          },
          { title: '入口标签', dataIndex: 'tag' },
          {
            title: '入口服务器',
            render: (_, row) =>
              row.ingress_id ? (
                <Link to={`/agents/${row.ingress_id}`}>{row.ingress_name || row.ingress_id}</Link>
              ) : (
                '无匹配服务器'
              ),
          },
          { title: '对外端口', dataIndex: 'port' },
          {
            title: '状态',
            render: (_, row) => (
              <Tag color={{ pending: 'default', ready: 'success', error: 'error' }[row.state]}>
                {{ pending: '等待中', ready: '可用', error: '失败' }[row.state]}
              </Tag>
            ),
          },
          {
            title: '原因',
            render: (_, row) => row.reason || (row.state === 'pending' ? '等待入口确认监听' : '—'),
          },
        ]}
      />
    </Card>
  );
}
