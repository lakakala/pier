import React, { useEffect, useState } from 'react';
import { createRoot } from 'react-dom/client';
import {
  BrowserRouter,
  Link,
  Navigate,
  Route,
  Routes,
  useLocation,
  useNavigate,
} from 'react-router-dom';
import {
  App as AntApp,
  Alert,
  Button,
  Card,
  ConfigProvider,
  Collapse,
  Divider,
  Form,
  Input,
  Layout,
  Menu,
  Space,
  Typography,
} from 'antd';
import zhCN from 'antd/locale/zh_CN';
import {
  ApartmentOutlined,
  AppstoreOutlined,
  CloudServerOutlined,
  DashboardOutlined,
  DeploymentUnitOutlined,
  GithubOutlined,
  LogoutOutlined,
  SettingOutlined,
} from '@ant-design/icons';
import { api, setSession, type Session } from './api';
import { ErrorBox, Loading } from './components';
import {
  Overview,
  Repository,
  Definitions,
  Agents,
  AgentDetail,
  Deployments,
  DeploymentDetail,
  Enrollment,
  Settings,
} from './pages';
import {
  ControllerSettings,
  RuntimeFields,
  runtimeBody,
  type AuthStatus,
  type SetupDefaults,
} from './runtime-settings';
import './style.css';
import { GlobalVariables } from './variables';

function Login({
  initialized,
  repositoryConfigured,
  setupDefaults,
  onLogin,
  onInitialized,
}: {
  initialized: boolean;
  repositoryConfigured: boolean;
  setupDefaults: SetupDefaults | null;
  onLogin: (s: Session) => void;
  onInitialized: () => void;
}) {
  const [error, setError] = useState<Error>();
  const [pending, setPending] = useState(false);
  return (
    <div className="auth-shell">
      <div className="auth-brand">
        <div className="wordmark">
          PIER<span>服务管理</span>
        </div>
        <h1>让服务有序运行。</h1>
        <p>从定义、构建到部署，集中管理你的服务器。</p>
      </div>
      <Card className="auth-card" title={initialized ? '登录控制台' : '初始化控制器'}>
        {!initialized && (
          <Alert
            type="info"
            title="创建管理员并配置定义仓库，完成后请到定义仓库页面手动同步。"
            className="block-gap"
          />
        )}
        <ErrorBox error={error} />
        <Form
          layout="vertical"
          requiredMark={false}
          initialValues={
            !initialized
              ? {
                  ...setupDefaults,
                  public_url: setupDefaults?.public_url || window.location.origin,
                }
              : undefined
          }
          onFinish={async (values) => {
            setPending(true);
            setError(undefined);
            try {
              const session = await api<Session>(
                `/v1/auth/${initialized ? 'login' : 'init'}`,
                'POST',
                {
                  username: values.username,
                  password: values.password,
                  ...(!initialized ? { settings: runtimeBody(values) } : {}),
                  ...(!initialized && !repositoryConfigured
                    ? {
                        repository: {
                          url: values.repository_url,
                          reference: values.repository_reference,
                        },
                      }
                    : {}),
                },
                false,
              );
              onLogin(session);
            } catch (e) {
              setError(e as Error);
              if (!initialized) onInitialized();
            } finally {
              setPending(false);
            }
          }}
        >
          {!initialized && <Divider titlePlacement="start">管理员</Divider>}
          <Form.Item
            name="username"
            label="用户名"
            rules={[
              { required: true, message: '请输入用户名' },
              { max: 64 },
              {
                validator: (_, v) =>
                  !v ||
                  (new TextEncoder().encode(v).length <= 64 && v.trim() === v && !/\p{Cc}/u.test(v))
                    ? Promise.resolve()
                    : Promise.reject(new Error('用户名不能含首尾空白或控制字符，且不超过 64 字节')),
              },
            ]}
          >
            <Input autoComplete="username" autoFocus />
          </Form.Item>
          <Form.Item
            name="password"
            label="密码"
            rules={[
              { required: true, message: '请输入密码' },
              ...(!initialized
                ? [
                    {
                      validator: (_: unknown, v: string) =>
                        v && [...v].length >= 12 && new TextEncoder().encode(v).length <= 1024
                          ? Promise.resolve()
                          : Promise.reject(new Error('密码至少 12 个字符，最多 1024 字节')),
                    },
                  ]
                : []),
            ]}
          >
            <Input.Password autoComplete={initialized ? 'current-password' : 'new-password'} />
          </Form.Item>
          {!initialized && (
            <Form.Item
              name="confirm"
              label="确认密码"
              dependencies={['password']}
              rules={[
                { required: true, message: '请再次输入密码' },
                ({ getFieldValue }) => ({
                  validator: (_, v) =>
                    v === getFieldValue('password')
                      ? Promise.resolve()
                      : Promise.reject(new Error('两次密码不一致')),
                }),
              ]}
            >
              <Input.Password autoComplete="new-password" />
            </Form.Item>
          )}
          {!initialized && !repositoryConfigured && (
            <>
              <Divider titlePlacement="start">定义仓库</Divider>
              <Form.Item
                name="repository_url"
                label="仓库地址"
                rules={[
                  { required: true, whitespace: true, message: '请输入 Git 仓库地址' },
                  { max: 4096 },
                ]}
              >
                <Input autoComplete="off" placeholder="https://git.example.com/services.git" />
              </Form.Item>
              <Form.Item
                name="repository_reference"
                label="分支或引用"
                initialValue="main"
                rules={[{ required: true, whitespace: true }, { max: 256 }]}
              >
                <Input />
              </Form.Item>
            </>
          )}
          {!initialized && (
            <Collapse
              className="block-gap"
              items={[
                {
                  key: 'runtime',
                  label: '运行设置',
                  forceRender: true,
                  children: (
                    <RuntimeFields
                      initialization
                      proxyConfigured={setupDefaults?.proxy_configured}
                    />
                  ),
                },
              ]}
            />
          )}
          <Button block type="primary" htmlType="submit" size="large" loading={pending}>
            {initialized ? '登录' : '创建管理员'}
          </Button>
        </Form>
      </Card>
    </div>
  );
}
function Console() {
  const [initialized, setInitialized] = useState<boolean>();
  const [repositoryConfigured, setRepositoryConfigured] = useState(false);
  const [setupDefaults, setSetupDefaults] = useState<SetupDefaults | null>(null);
  const [session, updateSession] = useState<Session | null>();
  const [error, setError] = useState<Error>();
  const location = useLocation();
  const navigate = useNavigate();
  const { message } = AntApp.useApp();
  const refreshStatus = () => {
    void api<AuthStatus>('/v1/auth/status', 'GET', undefined, false)
      .then((v) => {
        setInitialized(v.initialized);
        setRepositoryConfigured(v.repository_configured);
        setSetupDefaults(v.setup_defaults);
      })
      .catch(setError);
  };
  const accept = (value: Session | null) => {
    setSession(value);
    updateSession(value);
    if (value) setInitialized(true);
  };
  useEffect(() => {
    let active = true;
    void (async () => {
      try {
        const status = await api<AuthStatus>('/v1/auth/status', 'GET', undefined, false);
        let current: Session | null = null;
        if (status.initialized) {
          try {
            current = await api<Session>('/v1/auth/session', 'GET', undefined, false);
          } catch (e) {
            if ((e as { status?: number }).status !== 401) throw e;
          }
        }
        if (active) {
          setInitialized(status.initialized);
          setRepositoryConfigured(status.repository_configured);
          setSetupDefaults(status.setup_defaults);
          accept(current);
        }
      } catch (e) {
        if (active) setError(e as Error);
      }
    })();
    const expired = () => {
      accept(null);
      void message.warning('登录已失效，请重新登录');
    };
    window.addEventListener('pier-session-expired', expired);
    return () => {
      active = false;
      window.removeEventListener('pier-session-expired', expired);
    };
  }, []);
  useEffect(() => {
    // Keep enrollment fragments in the original URL, even while authenticating.
    if (location.pathname === '/agent/init' || initialized === undefined || session === undefined)
      return;
    if (!initialized && location.pathname !== '/init') navigate('/init', { replace: true });
    else if (initialized && !session && location.pathname === '/init')
      navigate('/login', { replace: true });
  }, [initialized, session, location.pathname, navigate]);
  if (error)
    return (
      <div className="loading">
        <ErrorBox error={error} retry={() => window.location.reload()} />
      </div>
    );
  if (initialized === undefined || session === undefined) return <Loading />;
  if (!session)
    return (
      <Login
        initialized={initialized}
        repositoryConfigured={repositoryConfigured}
        setupDefaults={setupDefaults}
        onLogin={accept}
        onInitialized={refreshStatus}
      />
    );
  const menu = [
    ['/', '概览', <DashboardOutlined />],
    ['/repository', '定义仓库', <GithubOutlined />],
    ['/apps', '应用', <AppstoreOutlined />],
    ['/blueprints', 'Blueprint', <ApartmentOutlined />],
    ['/variables', '全局变量', <AppstoreOutlined />],
    ['/agents', '服务器', <CloudServerOutlined />],
    ['/deployments', '部署记录', <DeploymentUnitOutlined />],
    ['/settings/controller', '控制器设置', <SettingOutlined />],
    ['/settings', '账号设置', <SettingOutlined />],
  ] as const;
  const selected =
    location.pathname === '/settings/controller'
      ? location.pathname
      : location.pathname === '/'
        ? '/'
        : `/${location.pathname.split('/')[1]}`;
  return (
    <Layout className="shell">
      <Layout.Sider breakpoint="lg" collapsedWidth={0} width={220} theme="light">
        <Link className="wordmark sidebar-brand" to="/">
          PIER<span>服务控制台</span>
        </Link>
        <Menu
          mode="inline"
          selectedKeys={[selected]}
          items={menu.map(([key, label, icon]) => ({
            key,
            icon,
            label: <Link to={key}>{label}</Link>,
          }))}
        />
      </Layout.Sider>
      <Layout>
        <Layout.Header className="topbar">
          <Typography.Text type="secondary">部署与运行</Typography.Text>
          <Space>
            <Typography.Text>{session.username}</Typography.Text>
            <Button
              icon={<LogoutOutlined aria-hidden="true" />}
              onClick={async () => {
                try {
                  await api('/v1/auth/logout', 'POST');
                  accept(null);
                  navigate('/login');
                } catch (e) {
                  void message.error((e as Error).message);
                }
              }}
            >
              退出
            </Button>
          </Space>
        </Layout.Header>
        <Layout.Content className="content">
          <Routes>
            <Route path="/" element={<Overview />} />
            <Route path="/repository" element={<Repository />} />
            <Route path="/apps" element={<Definitions kind="apps" />} />
            <Route path="/blueprints" element={<Definitions kind="blueprints" />} />
            <Route path="/variables" element={<GlobalVariables />} />
            <Route path="/agents" element={<Agents />} />
            <Route path="/agents/:id" element={<AgentDetail />} />
            <Route path="/deployments" element={<Deployments />} />
            <Route path="/deployments/:id" element={<DeploymentDetail />} />
            <Route path="/agent/init" element={<Enrollment />} />
            <Route
              path="/settings"
              element={
                <Settings
                  onChanged={() => {
                    accept(null);
                    navigate('/login');
                  }}
                />
              }
            />
            <Route path="/settings/controller" element={<ControllerSettings />} />
            <Route path="/init" element={<Navigate to="/" replace />} />
            <Route path="/login" element={<Navigate to="/" replace />} />
            <Route path="*" element={<Alert title="页面不存在" type="warning" />} />
          </Routes>
        </Layout.Content>
      </Layout>
    </Layout>
  );
}
const nonce = document.querySelector<HTMLMetaElement>('meta[name="csp-nonce"]')?.content;
createRoot(document.getElementById('root')!).render(
  <React.StrictMode>
    <ConfigProvider
      locale={zhCN}
      button={{ autoInsertSpace: false }}
      csp={{ nonce: nonce ?? '' }}
      theme={{
        token: {
          colorPrimary: '#2563eb',
          borderRadius: 8,
          fontFamily: 'Inter, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif',
        },
      }}
    >
      <AntApp>
        <BrowserRouter>
          <Console />
        </BrowserRouter>
      </AntApp>
    </ConfigProvider>
  </React.StrictMode>,
);
