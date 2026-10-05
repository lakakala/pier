import { useEffect, useState } from 'react';
import {
  Alert,
  App,
  Button,
  Card,
  Form,
  Input,
  InputNumber,
  Select,
  Space,
  Table,
  Typography,
} from 'antd';
import { api } from './api';
import { ErrorBox, Heading, Loading } from './components';

export interface RuntimeSettings {
  tcp_listen: string;
  public_url: string;
  agent_endpoint: string;
  max_concurrent_builds: number;
  build_proxy: { http_proxy: string | null; https_proxy: string | null; no_proxy: string | null };
}
export type SetupDefaults = Omit<RuntimeSettings, 'build_proxy'> & { proxy_configured: boolean };
export interface AuthStatus {
  initialized: boolean;
  repository_configured: boolean;
  setup_defaults: SetupDefaults | null;
}
export interface RuntimeView {
  active: RuntimeSettings;
  saved: RuntimeSettings;
  restart_required: boolean;
  agent_listener: { listening: boolean; error: string | null };
}
interface RuntimeValues extends Omit<RuntimeSettings, 'build_proxy'> {
  proxy_action?: string;
  http_proxy?: string;
  https_proxy?: string;
  no_proxy?: string;
}
export function runtimeBody(values: RuntimeValues) {
  return {
    tcp_listen: values.tcp_listen,
    public_url: values.public_url,
    agent_endpoint: values.agent_endpoint || '',
    max_concurrent_builds: values.max_concurrent_builds,
    ...(values.proxy_action === 'clear'
      ? { build_proxy: {} }
      : values.proxy_action === 'replace'
        ? {
            build_proxy: {
              http_proxy: values.http_proxy || null,
              https_proxy: values.https_proxy || null,
              no_proxy: values.no_proxy || null,
            },
          }
        : {}),
  };
}
export function RuntimeFields({
  initialization = false,
  proxyConfigured = false,
}: {
  initialization?: boolean;
  proxyConfigured?: boolean;
}) {
  return (
    <>
      <Form.Item
        name="tcp_listen"
        label="Agent 通信监听地址"
        rules={[{ required: true, whitespace: true }, { max: 128 }]}
        extra="IP:端口，例如 0.0.0.0:7443；IPv6 使用 [::]:7443。"
      >
        <Input autoComplete="off" />
      </Form.Item>
      <Form.Item
        name="public_url"
        label="Web 公开地址"
        rules={[{ required: true, whitespace: true }, { max: 4096 }]}
        extra={
          initialization
            ? '支持 HTTP 和 HTTPS；首次初始化须与当前访问来源一致，不含路径或末尾斜杠。'
            : '支持 HTTP 和 HTTPS；重启后请使用此地址访问控制台。'
        }
      >
        <Input autoComplete="off" />
      </Form.Item>
      <Form.Item
        name="agent_endpoint"
        label="Agent 对外连接地址"
        rules={[...(!initialization ? [{ required: true, whitespace: true }] : []), { max: 1024 }]}
        extra={
          initialization
            ? '可留空，默认使用当前主机名和通信监听端口；也可填写单独的域名:端口。'
            : '修改后需相应调整已有 agent 的连接配置。'
        }
      >
        <Input autoComplete="off" placeholder="pier.example.com:7443" />
      </Form.Item>
      <Form.Item
        name="max_concurrent_builds"
        label="构建并发数"
        rules={[{ required: true }, { type: 'integer', min: 1, max: 64 }]}
      >
        <InputNumber min={1} max={64} precision={0} style={{ width: '100%' }} />
      </Form.Item>
      <Form.Item
        name="proxy_action"
        label="构建代理"
        initialValue="preserve"
        extra="是否使用代理由 app 的 YML 控制；Docker 拉取镜像的代理仍由 Docker daemon 配置。"
      >
        <Select
          options={[
            { value: 'preserve', label: proxyConfigured ? '保留已有代理' : '不配置代理' },
            { value: 'replace', label: '设置代理' },
            { value: 'clear', label: '清除代理' },
          ]}
        />
      </Form.Item>
      <Form.Item noStyle shouldUpdate={(a, b) => a.proxy_action !== b.proxy_action}>
        {({ getFieldValue }) =>
          getFieldValue('proxy_action') === 'replace' && (
            <>
              <Form.Item name="http_proxy" label="HTTP 代理" rules={[{ max: 4096 }]}>
                <Input.Password autoComplete="off" placeholder="http://proxy.example.com:7890" />
              </Form.Item>
              <Form.Item name="https_proxy" label="HTTPS 代理" rules={[{ max: 4096 }]}>
                <Input.Password autoComplete="off" placeholder="http://proxy.example.com:7890" />
              </Form.Item>
              <Form.Item name="no_proxy" label="NO_PROXY" rules={[{ max: 4096 }]}>
                <Input autoComplete="off" placeholder="localhost,127.0.0.1,.internal" />
              </Form.Item>
            </>
          )
        }
      </Form.Item>
    </>
  );
}
function configured(value: RuntimeSettings) {
  return Object.values(value.build_proxy).some((v) => !!v);
}
export function ControllerSettings() {
  const [view, setView] = useState<RuntimeView>();
  const [error, setError] = useState<Error>();
  const [pending, setPending] = useState(false);
  const [form] = Form.useForm<RuntimeValues>();
  const { message } = App.useApp();
  const accept = (value: RuntimeView) => {
    setView(value);
    form.setFieldsValue({
      ...value.saved,
      proxy_action: 'preserve',
      http_proxy: '',
      https_proxy: '',
      no_proxy: '',
    });
  };
  const refresh = async () => {
    setPending(true);
    setError(undefined);
    try {
      accept(await api<RuntimeView>('/v1/settings'));
    } catch (e) {
      setError(e as Error);
    } finally {
      setPending(false);
    }
  };
  useEffect(() => {
    void refresh();
  }, []);
  return (
    <>
      <Heading
        title="控制器设置"
        subtitle="修改运行设置后需手动重启服务。仓库配置和同步请前往定义仓库页面。"
      />
      <ErrorBox error={error} retry={() => void refresh()} />
      {!view ? (
        !error && <Loading />
      ) : (
        <>
          {view.agent_listener.error && (
            <Alert
              className="block-gap"
              type="error"
              title="Agent 通信不可用"
              description="请检查监听地址、端口占用及权限，修改设置后重启控制器。Web 控制台仍可正常使用。"
            />
          )}
          {view.restart_required && (
            <Alert
              className="block-gap"
              type="warning"
              title="已保存，重启后生效"
              description={
                <>
                  <div>
                    请在维护窗口执行{' '}
                    <Typography.Text code>sudo systemctl restart pier-controller</Typography.Text>
                    。当前监听和构建仍使用原配置。
                  </div>
                  <div>
                    公开地址修改后，请使用 {view.saved.public_url} 访问；通信地址修改后需调整已有
                    agent 配置。
                  </div>
                </>
              }
            />
          )}
          <Card title="当前生效与已保存配置" className="block-gap">
            <Table
              pagination={false}
              rowKey="name"
              size="small"
              scroll={{ x: true }}
              columns={[
                { title: '设置', dataIndex: 'name' },
                { title: '当前生效', dataIndex: 'active' },
                { title: '已保存', dataIndex: 'saved' },
              ]}
              dataSource={[
                ...(
                  [
                    ['tcp_listen', 'Agent 监听'],
                    ['public_url', 'Web 公开地址'],
                    ['agent_endpoint', 'Agent 连接地址'],
                    ['max_concurrent_builds', '构建并发数'],
                  ] as const
                ).map(([key, name]) => ({
                  name,
                  active: view.active[key],
                  saved: view.saved[key],
                })),
                {
                  name: '构建代理',
                  active: configured(view.active) ? '已配置' : '未配置',
                  saved: `${configured(view.saved) ? '已配置' : '未配置'}${JSON.stringify(view.active.build_proxy) !== JSON.stringify(view.saved.build_proxy) ? '（待变更）' : ''}`,
                },
              ]}
            />
          </Card>
        </>
      )}
      <Card
        title="运行设置"
        className="settings-card"
        style={{ display: view ? undefined : 'none' }}
      >
        <Form
          form={form}
          layout="vertical"
          disabled={pending}
          onFinish={async (values) => {
            setPending(true);
            setError(undefined);
            try {
              const updated = await api<RuntimeView>('/v1/settings', 'PUT', runtimeBody(values));
              accept(updated);
              void message.success(
                updated.restart_required ? '设置已保存，重启后生效' : '设置与当前配置一致',
              );
            } catch (e) {
              setError(e as Error);
            } finally {
              setPending(false);
            }
          }}
        >
          <RuntimeFields proxyConfigured={view ? configured(view.saved) : false} />
          <Space>
            <Button type="primary" htmlType="submit" aria-label="保存运行设置" loading={pending}>
              保存运行设置
            </Button>
            <Button onClick={() => void refresh()} disabled={pending}>
              重新加载
            </Button>
          </Space>
        </Form>
      </Card>
    </>
  );
}
