import {
  AgentConnection,
  connectionLabel,
  ConnectionProxy,
  proxyValue,
  type ProxyMode,
} from './connection';
import { useEffect, useMemo, useRef, useState } from 'react';
import { Link, useLocation, useNavigate, useParams } from 'react-router-dom';
import {
  Alert,
  App,
  Button,
  Card,
  Checkbox,
  Col,
  Descriptions,
  Drawer,
  Empty,
  Form,
  Input,
  Modal,
  Row,
  Select,
  Space,
  Statistic,
  Table,
  Tag,
  Typography,
} from 'antd';
import {
  api,
  date,
  packageVersion,
  upgrading,
  upgradeLabel,
  upgradeReason,
  terminal,
  type Agent,
  type Binding,
  type Blueprint,
  type Catalog,
  type Job,
} from './api';
import { ErrorBox, Heading, Loading, StateTag, useLoad, Variables } from './components';
import type { RuntimeView } from './runtime-settings';
import { AppTerminal } from './terminal';

type RepositoryInfo = {
  repository: { url: string; reference: string } | null;
  commit: string | null;
  error: string | null;
  needs_sync: boolean;
};
export function Overview() {
  const runtime = useLoad<RuntimeView>('/v1/settings', 5000);
  const agents = useLoad<{ agents: Agent[] }>('/v1/agents', 5000);
  const jobs = useLoad<{ deployments: Job[] }>('/v1/deployments', 5000);
  const repo = useLoad<RepositoryInfo>('/v1/repository', 5000);
  return (
    <>
      <Heading
        title="运行概览"
        subtitle="定义、服务器与部署，一处掌握。"
        actions={
          <Link to="/agents">
            <Button type="primary">管理服务器</Button>
          </Link>
        }
      />
      <ErrorBox error={agents.error || jobs.error || repo.error} />
      {runtime.data?.agent_listener.error && (
        <Alert
          type="error"
          className="block-gap"
          title="Agent 通信不可用"
          description={<Link to="/settings/controller">前往控制器设置修复监听配置</Link>}
        />
      )}
      {runtime.data?.restart_required && (
        <Alert
          type="warning"
          className="block-gap"
          title="运行设置待重启生效"
          description={<Link to="/settings/controller">查看已保存设置</Link>}
        />
      )}
      <Row gutter={[20, 20]} className="block-gap">
        {[
          ['服务器', agents.data?.agents.length ?? 0],
          ['在线服务器', agents.data?.agents.filter((a) => a.online).length ?? 0],
          [
            '进行中的部署',
            jobs.data?.deployments.filter((j) => !terminal.has(j.state)).length ?? 0,
          ],
          ['部署记录', jobs.data?.deployments.length ?? 0],
        ].map(([title, value]) => (
          <Col xs={24} sm={12} xl={6} key={title}>
            <Card>
              <Statistic title={title} value={value} />
            </Card>
          </Col>
        ))}
      </Row>
      <Card title="定义仓库" extra={<Link to="/repository">查看仓库</Link>} className="block-gap">
        <Typography.Text code>{repo.data?.commit ?? '尚无可用目录'}</Typography.Text>
        {repo.data?.needs_sync && (
          <Alert
            className="block-gap"
            type="warning"
            title="仓库待同步，请前往定义仓库页面手动同步"
          />
        )}
        {repo.data?.error && <Alert className="block-gap" type="error" title={repo.data.error} />}
      </Card>
      <Card title="最近部署">
        <JobTable jobs={jobs.data?.deployments ?? []} limit={5} />
      </Card>
    </>
  );
}
export function Repository() {
  const repo = useLoad<RepositoryInfo>('/v1/repository', 5000);
  const [pending, setPending] = useState(false);
  const [error, setError] = useState<Error>();
  const [form] = Form.useForm();
  const { message } = App.useApp();
  const loadedRepository = useRef<string>(undefined);
  useEffect(() => {
    if (!repo.data) return;
    const value = JSON.stringify(repo.data.repository);
    if (loadedRepository.current === value) return;
    loadedRepository.current = value;
    form.setFieldsValue(repo.data.repository ?? { url: '', reference: 'main' });
  }, [repo.data?.repository?.url, repo.data?.repository?.reference, form]);
  return (
    <>
      <Heading
        title="定义仓库"
        subtitle="保存配置后点击立即同步。启动、重启和保存配置均不会自动拉取仓库。"
        actions={
          <Button
            type="primary"
            aria-label="立即同步"
            aria-busy={pending}
            loading={pending}
            disabled={pending || !repo.data?.repository}
            onClick={async () => {
              setPending(true);
              setError(undefined);
              try {
                await api('/v1/repository/sync', 'POST');
                void message.success('仓库同步完成');
              } catch (e) {
                setError(e as Error);
              } finally {
                setPending(false);
                repo.refresh();
              }
            }}
          >
            立即同步
          </Button>
        }
      />
      <ErrorBox error={error || repo.error} retry={repo.refresh} />
      {repo.data?.needs_sync && (
        <Alert
          className="block-gap"
          type="warning"
          title="待同步"
          description="当前仓库配置尚未同步成功，暂时不能创建新部署。已有服务继续运行。"
        />
      )}
      <Card title="仓库配置" className="block-gap">
        <Typography.Paragraph type="secondary">
          HTTP(S) 仓库同步使用<Link to="/settings/controller">控制器设置中的代理</Link>
          ，未配置对应代理时直连。保存或清除代理后需重启生效。
        </Typography.Paragraph>
        <Form
          form={form}
          layout="vertical"
          onFinish={async (values) => {
            setPending(true);
            setError(undefined);
            try {
              await api('/v1/repository', 'PUT', values);
              void message.success('仓库配置已保存，请手动同步');
            } catch (e) {
              setError(e as Error);
            } finally {
              setPending(false);
              repo.refresh();
            }
          }}
        >
          <Form.Item
            name="url"
            label="仓库地址"
            rules={[{ required: true, whitespace: true }, { max: 4096 }]}
          >
            <Input autoComplete="off" />
          </Form.Item>
          <Form.Item
            name="reference"
            label="分支或引用"
            rules={[{ required: true, whitespace: true }, { max: 256 }]}
          >
            <Input />
          </Form.Item>
          <Button
            htmlType="submit"
            aria-label="保存配置"
            aria-busy={pending}
            loading={pending}
            disabled={pending}
          >
            保存配置
          </Button>
        </Form>
      </Card>
      <Card title="同步状态">
        <Descriptions
          column={1}
          items={[
            {
              key: 'commit',
              label: '最近成功 commit',
              children: (
                <Typography.Text copyable={!!repo.data?.commit} code>
                  {repo.data?.commit ?? '尚无可用目录'}
                </Typography.Text>
              ),
            },
            {
              key: 'error',
              label: '最近同步',
              children: repo.data?.error ? (
                <Alert type="error" title={repo.data.error} />
              ) : repo.data?.needs_sync ? (
                '待手动同步'
              ) : (
                '正常'
              ),
            },
          ]}
        />
      </Card>
      <Alert
        className="block-gap"
        type="info"
        title="定义由 Git 维护"
        description="修改仓库中的 pier-pkg.yml 或 pier-blueprint.yml 后手动同步；同步成功不会自动部署。"
      />
    </>
  );
}
export function Definitions({ kind }: { kind: 'apps' | 'blueprints' }) {
  const catalog = useLoad<Catalog>(`/v1/${kind}`);
  const [selected, setSelected] = useState<string>();
  const [search, setSearch] = useState('');
  const definitions = catalog.data?.[kind] ?? {};
  const definition = selected ? definitions[selected] : undefined;
  return (
    <>
      <Heading
        title={kind === 'apps' ? '应用定义' : 'Blueprint'}
        subtitle={catalog.data ? `目录版本 ${catalog.data.commit}` : '读取 Git 中的服务定义'}
      />
      <ErrorBox error={catalog.error} retry={catalog.refresh} />
      <Card>
        <Input.Search
          placeholder="搜索名称或路径"
          allowClear
          value={search}
          onChange={(e) => setSearch(e.target.value)}
          className="search"
        />
        <Table
          rowKey="id"
          loading={!catalog.data && !catalog.error}
          dataSource={Object.entries(definitions)
            .map(([id, value]) => ({ id, ...value }))
            .filter((v) => `${v.id} ${v.name}`.toLowerCase().includes(search.toLowerCase()))}
          columns={[
            {
              title: '名称',
              dataIndex: 'name',
              render: (name: string, row: { id: string }) => (
                <Button type="link" onClick={() => setSelected(row.id)}>
                  {name}
                </Button>
              ),
            },
            { title: '定义路径', dataIndex: 'id' },
            ...(kind === 'apps'
              ? [
                  { title: '版本', dataIndex: 'version' },
                  { title: '来源', dataIndex: 'source' },
                ]
              : []),
          ]}
        />
      </Card>
      <Drawer
        title={definition?.name}
        open={!!definition}
        onClose={() => setSelected(undefined)}
        size="large"
        destroyOnHidden
      >
        {definition && (
          <>
            <Typography.Paragraph code>{selected}</Typography.Paragraph>
            <Typography.Title level={4}>变量声明</Typography.Title>
            <Variables variables={definition.variables} />
            {kind === 'blueprints' &&
              (definition as Blueprint).apps.map((app) => (
                <Card key={app.id} title={app.id} className="block-gap">
                  <Typography.Paragraph>{app.app}</Typography.Paragraph>
                  <Typography.Text type="secondary">变量映射</Typography.Text>
                  <pre>{JSON.stringify(app.variables, null, 2)}</pre>
                  <Typography.Text type="secondary">应用全部变量</Typography.Text>
                  <Variables variables={catalog.data?.apps[app.app]?.variables ?? {}} />
                </Card>
              ))}
          </>
        )}
      </Drawer>
    </>
  );
}
export function Agents() {
  const agents = useLoad<{ agents: Agent[] }>('/v1/agents', 5000);
  const [open, setOpen] = useState(false);
  const [pending, setPending] = useState(false);
  const [error, setError] = useState<Error>();
  const [registered, setRegistered] = useState<{ id: string; token: string }>();
  return (
    <>
      <Heading
        title="服务器"
        subtitle="运行 pier-agent init 接入服务器，或手动注册身份。"
        actions={
          <Button
            onClick={() => {
              setOpen(true);
              setError(undefined);
            }}
          >
            手动注册
          </Button>
        }
      />
      <ErrorBox error={agents.error} retry={agents.refresh} />
      <Card>
        <Table
          rowKey="id"
          loading={!agents.data && !agents.error}
          dataSource={agents.data?.agents}
          columns={[
            {
              title: '名称',
              dataIndex: 'name',
              render: (value: string, a: Agent) => <Link to={`/agents/${a.id}`}>{value}</Link>,
            },
            {
              title: '状态',
              dataIndex: 'online',
              render: (v: boolean) => (
                <Tag color={v ? 'success' : 'default'}>{v ? '在线' : '离线'}</Tag>
              ),
            },
            { title: '连接方式', render: (_, a) => connectionLabel(a) },
            { title: '主机', render: (_, a) => a.info?.hostname ?? '—' },
            { title: '架构', render: (_, a) => a.info?.architecture ?? '—' },
            { title: 'Agent 版本', render: (_, a) => a.software?.version ?? '未知' },
            {
              title: '自动升级',
              render: (_, a) => (
                <Space orientation="vertical" size={0}>
                  <span>{upgradeLabel(a)}</span>
                  {upgradeReason(a) && (
                    <Typography.Text type="secondary">{upgradeReason(a)}</Typography.Text>
                  )}
                </Space>
              ),
            },
            { title: '最近上报', dataIndex: 'last_seen', render: date },
          ]}
        />
      </Card>
      <Modal
        title={registered ? '保存 agent 凭据' : '手动注册 agent'}
        open={open}
        footer={null}
        destroyOnHidden
        onCancel={() => {
          setOpen(false);
          setRegistered(undefined);
        }}
      >
        {registered ? (
          <>
            <Alert
              type="warning"
              title="此凭据仅在本次注册时展示，请保存到 agent 的 token_file。"
            />
            <Descriptions
              column={1}
              items={[
                {
                  key: 'id',
                  label: 'agent_id',
                  children: <Typography.Text copyable>{registered.id}</Typography.Text>,
                },
                {
                  key: 'token',
                  label: 'token',
                  children: <Typography.Text copyable>{registered.token}</Typography.Text>,
                },
              ]}
            />
          </>
        ) : (
          <>
            <ErrorBox error={error} />
            <Form
              layout="vertical"
              onFinish={async (values) => {
                setPending(true);
                try {
                  setRegistered(await api('/v1/agents', 'POST', values));
                  agents.refresh();
                } catch (e) {
                  setError(e as Error);
                } finally {
                  setPending(false);
                }
              }}
            >
              <Form.Item name="name" label="服务器名称" rules={[{ required: true }, { max: 256 }]}>
                <Input />
              </Form.Item>
              <Button type="primary" htmlType="submit" loading={pending}>
                注册
              </Button>
            </Form>
          </>
        )}
      </Modal>
    </>
  );
}
function BindingForm({ agent }: { agent: Agent }) {
  const bindings = useLoad<{ bindings: Binding[] }>(`/v1/agents/${agent.id}/bindings`);
  const catalog = useLoad<Catalog>('/v1/blueprints');
  const [selected, setSelected] = useState('');
  const [replace, setReplace] = useState(false);
  const binding = {
    data: bindings.data?.bindings.find((b) => b.blueprint === selected),
    error: bindings.error,
    refresh: bindings.refresh,
  };
  const navigate = useNavigate();
  const [modes, setModes] = useState<string[]>([]);
  const [error, setError] = useState<Error>();
  const [pending, setPending] = useState(false);
  const [form] = Form.useForm();
  const { message } = App.useApp();
  useEffect(() => {
    if (!selected && bindings.data?.bindings.length)
      setSelected(bindings.data.bindings[0].blueprint);
  }, [bindings.data, selected]);
  const blueprint = catalog.data?.blueprints[selected];
  const editing = selected === binding.data?.blueprint && !replace;
  const variables = Object.entries(blueprint?.variables ?? {});
  useEffect(() => {
    form.resetFields();
    setModes(
      Object.entries(blueprint?.variables ?? {}).map(([name, def]) =>
        editing && binding.data?.variable_names.includes(name)
          ? 'keep'
          : def.default !== null
            ? 'default'
            : 'set',
      ),
    );
  }, [blueprint, editing, binding.data, form]);
  const failedBinding = binding.error?.status !== 404 ? binding.error : undefined;
  return (
    <Card title="Blueprint 绑定" className="block-gap">
      <ErrorBox error={error || catalog.error || failedBinding} />
      <Table<Binding>
        rowKey="id"
        pagination={false}
        dataSource={bindings.data?.bindings ?? []}
        columns={[
          { title: '蓝图', dataIndex: 'blueprint' },
          {
            title: '用户 / 组',
            render: (_, b) =>
              agent.report.blueprints?.find((p) => p.id === b.id)?.name ??
              catalog.data?.blueprints[b.blueprint]?.name ??
              '—',
          },
          {
            title: '状态',
            render: (_, b) => (
              <StateTag
                state={agent.report.blueprints?.find((p) => p.id === b.id)?.state ?? '未部署'}
              />
            ),
          },
          {
            title: '操作',
            render: (_, b) => {
              const deployed = agent.report.blueprints?.find((p) => p.id === b.id);
              const available =
                agent.online &&
                !upgrading(agent) &&
                agent.report.capabilities?.includes('multi_blueprint_v1');
              return (
                <Space wrap>
                  <Button
                    onClick={() => {
                      setSelected(b.blueprint);
                      setReplace(false);
                    }}
                  >
                    编辑变量
                  </Button>
                  <Button
                    disabled={!available || !deployed || deployed.state === 'stopped' || pending}
                    onClick={async () => {
                      setPending(true);
                      setError(undefined);
                      try {
                        const job = await api<{ id: string }>('/v1/deployments', 'POST', {
                          agent_id: agent.id,
                          blueprint: b.blueprint,
                          action: 'stop',
                        });
                        navigate(`/deployments/${job.id}`);
                      } catch (e) {
                        setError(e as Error);
                      } finally {
                        setPending(false);
                      }
                    }}
                  >
                    停止蓝图
                  </Button>
                  <Button
                    disabled={!available || (!!deployed && deployed.state !== 'stopped') || pending}
                    onClick={async () => {
                      setPending(true);
                      setError(undefined);
                      try {
                        await api(`/v1/agents/${agent.id}/bindings/${b.id}`, 'DELETE');
                        bindings.refresh();
                        if (selected === b.blueprint) setSelected('');
                        void message.success('已解除绑定，用户和数据保留');
                      } catch (e) {
                        setError(e as Error);
                      } finally {
                        setPending(false);
                      }
                    }}
                  >
                    解除绑定
                  </Button>
                </Space>
              );
            },
          },
        ]}
      />
      <Typography.Paragraph className="block-gap" type="secondary">
        每个蓝图独立部署；同蓝图应用共用账户和数据，任一应用退出会重启整个蓝图。先停止蓝图，再解除绑定。
      </Typography.Paragraph>
      <Form
        layout="vertical"
        form={form}
        onFinish={async (values) => {
          if (!blueprint) return;
          setPending(true);
          setError(undefined);
          const updates = Object.fromEntries(
            variables.flatMap(([name], index) =>
              modes[index] === 'keep'
                ? []
                : modes[index] === 'default'
                  ? editing
                    ? [[name, null]]
                    : []
                  : [[name, values.items?.[index]?.value ?? '']],
            ),
          );
          try {
            await api(
              `/v1/agents/${agent.id}/bindings${binding.data ? `/${binding.data.id}` : ''}`,
              binding.data ? (editing ? 'PATCH' : 'PUT') : 'POST',
              {
                blueprint: selected,
                variables: updates,
              },
            );
            void message.success('绑定已保存，创建部署后应用到服务器');
            binding.refresh();
            setReplace(false);
          } catch (e) {
            setError(e as Error);
          } finally {
            setPending(false);
          }
        }}
      >
        <Form.Item label="选择 Blueprint">
          <Select
            aria-label="选择 Blueprint"
            showSearch
            optionFilterProp="label"
            value={selected || undefined}
            onChange={setSelected}
            options={Object.entries(catalog.data?.blueprints ?? {}).map(([value, bp]) => ({
              value,
              label: `${bp.name} · ${value}`,
            }))}
          />
        </Form.Item>
        {selected === binding.data?.blueprint && (
          <Checkbox
            className="block-gap"
            checked={replace}
            onChange={(e) => setReplace(e.target.checked)}
          >
            重新填写全部变量（替换原绑定值）
          </Checkbox>
        )}
        {blueprint &&
          variables.map(([name, def], index) => (
            <div key={name} className="variable-row">
              <Form.Item
                label={
                  <Space>
                    {name}
                    {def.default === null && <Tag color="orange">必填</Tag>}
                  </Space>
                }
              >
                <Select
                  aria-label={`${name} 的填写方式`}
                  value={modes[index]}
                  onChange={(value) =>
                    setModes((current) => current.map((v, i) => (i === index ? value : v)))
                  }
                  options={[
                    ...(editing && binding.data?.variable_names.includes(name)
                      ? [{ value: 'keep', label: '保留已保存值' }]
                      : []),
                    { value: 'set', label: '设置值' },
                    ...(def.default !== null
                      ? [{ value: 'default', label: `使用默认值：${JSON.stringify(def.default)}` }]
                      : []),
                  ]}
                />
              </Form.Item>
              {modes[index] === 'set' && (
                <Form.Item
                  name={['items', index, 'value']}
                  label={`${name} 的值`}
                  rules={[
                    {
                      validator: (_, v) =>
                        v !== undefined
                          ? Promise.resolve()
                          : Promise.reject(new Error('请输入值；允许空字符串')),
                    },
                  ]}
                >
                  <Input.TextArea rows={2} autoComplete="off" />
                </Form.Item>
              )}
            </div>
          ))}
        {blueprint && !variables.length && (
          <Typography.Paragraph type="secondary">此 blueprint 无需填写变量。</Typography.Paragraph>
        )}
        <Button
          htmlType="submit"
          type="primary"
          loading={pending}
          disabled={!blueprint || !!failedBinding || (!bindings.data && !bindings.error)}
        >
          保存绑定
        </Button>
      </Form>
    </Card>
  );
}
function DeployForm({ agent }: { agent: Agent }) {
  const [open, setOpen] = useState(false);
  return (
    <>
      <Button
        type="primary"
        disabled={
          !agent.online ||
          upgrading(agent) ||
          !agent.report.capabilities?.includes('multi_blueprint_v1')
        }
        onClick={() => setOpen(true)}
      >
        创建部署
      </Button>
      <Modal
        title="创建部署"
        open={open}
        footer={null}
        onCancel={() => setOpen(false)}
        destroyOnHidden
      >
        {open && <DeploymentEditor agent={agent} />}
      </Modal>
    </>
  );
}
function DeploymentEditor({ agent }: { agent: Agent }) {
  const repository = useLoad<RepositoryInfo>('/v1/repository');
  const bindings = useLoad<{ bindings: Binding[] }>(`/v1/agents/${agent.id}/bindings`);
  const [selected, setSelected] = useState('');
  const binding = bindings.data?.bindings.find((b) => b.blueprint === selected);
  const catalog = useLoad<Catalog>('/v1/blueprints');
  const navigate = useNavigate();
  const [error, setError] = useState<Error>();
  const [pending, setPending] = useState(false);
  const blueprint = binding && catalog.data?.blueprints[binding.blueprint];
  const sourceApps =
    blueprint?.apps.filter((app) => catalog.data?.apps[app.app]?.source === 'git') ?? [];
  return (
    <>
      <ErrorBox error={error || bindings.error || catalog.error || repository.error} />
      {repository.data?.needs_sync && <Alert type="warning" title="请先手动同步定义仓库" />}
      <Alert
        className="block-gap"
        type="info"
        title={`目标架构：${agent.info?.architecture ?? '未知'}`}
        description="每个源码实例需要填写兼容目标系统的 Docker 编译镜像。"
      />
      <Typography.Paragraph code>{catalog.data?.commit}</Typography.Paragraph>
      <Form
        key={selected}
        layout="vertical"
        onFinish={async (values) => {
          if (!blueprint || !catalog.data) return;
          setPending(true);
          setError(undefined);
          try {
            const job = await api<{ id: string }>('/v1/deployments', 'POST', {
              agent_id: agent.id,
              blueprint: selected,
              commit: catalog.data.commit,
              images: Object.fromEntries(
                sourceApps.map((app, index) => [app.id, values.images?.[index]]),
              ),
            });
            navigate(`/deployments/${job.id}`);
          } catch (e) {
            setError(e as Error);
            if ((e as { status?: number }).status === 409) {
              catalog.refresh();
              bindings.refresh();
            }
          } finally {
            setPending(false);
          }
        }}
      >
        <Form.Item label="部署的蓝图">
          <Select
            aria-label="部署的蓝图"
            value={selected || undefined}
            onChange={setSelected}
            options={(bindings.data?.bindings ?? []).map((b) => ({
              value: b.blueprint,
              label: `${catalog.data?.blueprints[b.blueprint]?.name ?? b.blueprint} · ${b.blueprint}`,
            }))}
          />
        </Form.Item>
        {sourceApps.map((app, index) => (
          <Form.Item
            key={app.id}
            name={['images', index]}
            label={`${app.id} 的构建镜像`}
            rules={[{ required: true, whitespace: true, message: '请输入镜像名或 digest' }]}
          >
            <Input placeholder="例如 pier-builder-rust:almalinux8" />
          </Form.Item>
        ))}
        {blueprint && !sourceApps.length && (
          <Typography.Paragraph>此 blueprint 无需编译镜像。</Typography.Paragraph>
        )}
        <Typography.Paragraph type="secondary">
          确认当前 commit 和镜像后提交。修改绑定或同步仓库不会自动执行部署。
        </Typography.Paragraph>
        <Button
          type="primary"
          htmlType="submit"
          loading={pending}
          disabled={
            !blueprint || !agent.online || upgrading(agent) || repository.data?.needs_sync !== false
          }
        >
          确认部署
        </Button>
      </Form>
    </>
  );
}
export function AgentDetail() {
  const { id } = useParams();
  const agent = useLoad<Agent>(`/v1/agents/${encodeURIComponent(id!)}`, 5000);
  if (!agent.data)
    return agent.error ? <ErrorBox error={agent.error} retry={agent.refresh} /> : <Loading />;
  const value = agent.data;
  return (
    <>
      <Heading
        title={value.name}
        subtitle={value.id}
        actions={
          <>
            <Link to="/agents">
              <Button>返回列表</Button>
            </Link>
            <DeployForm agent={value} />
          </>
        }
      />
      <ErrorBox error={agent.error} />
      <Card title="服务器状态">
        <Descriptions
          column={{ xs: 1, md: 2 }}
          items={[
            {
              key: 'online',
              label: '连接',
              children: (
                <Tag color={value.online ? 'success' : 'default'}>
                  {value.online ? '在线' : '离线'}
                </Tag>
              ),
            },
            { key: 'direction', label: '连接方式', children: connectionLabel(value) },
            { key: 'arch', label: '架构', children: value.info?.architecture ?? '—' },
            { key: 'host', label: '主机', children: value.info?.hostname ?? '—' },
            { key: 'seen', label: '最近上报', children: date(value.last_seen) },
            {
              key: 'version',
              label: 'Agent 运行版本',
              children: value.software?.version ?? '未知',
            },
            {
              key: 'package',
              label: '已安装包版本',
              children: packageVersion(value.software?.package),
            },
            {
              key: 'target',
              label: '服务端目标版本',
              children: packageVersion(value.upgrade?.target?.package),
            },
            { key: 'upgrade', label: '自动升级', children: upgradeLabel(value) },
            { key: 'os', label: '发行信息', children: <pre>{value.info?.os_release ?? '—'}</pre> },
          ]}
        />
        <AgentConnection key={value.connection?.endpoint} agent={value} refresh={agent.refresh} />
        {upgrading(value) && (
          <Alert
            className="block-gap"
            type="info"
            title="Agent 正在升级，暂时无法创建部署；重启时应用会短暂中断。"
          />
        )}
        {upgradeReason(value) && (
          <Alert
            className="block-gap"
            type="warning"
            title="自动升级不可用原因"
            description={upgradeReason(value)}
          />
        )}
        {value.upgrade?.status?.error && (
          <Alert className="block-gap" type="warning" title={value.upgrade.status.error} />
        )}
        {!value.online && (
          <Alert type="warning" title="服务器离线，下方展示最近一次上报的进程信息。" />
        )}
        {!value.report.capabilities?.includes('multi_blueprint_v1') && (
          <Alert type="info" title="升级 agent 后可使用多蓝图部署" />
        )}
        {(value.report.blueprints ?? []).map((blueprint) => (
          <Card
            key={blueprint.id}
            className="block-gap"
            title={`${blueprint.name} · ${blueprint.blueprint}`}
            extra={<StateTag state={blueprint.state} />}
          >
            <Typography.Paragraph type="secondary">
              用户 / 组：{blueprint.name} · 共享数据目录
            </Typography.Paragraph>
            <Table
              rowKey="instance"
              pagination={false}
              dataSource={blueprint.apps}
              columns={[
                { title: '应用', dataIndex: 'id' },
                {
                  title: '状态',
                  dataIndex: 'state',
                  render: (state) => <StateTag state={state} />,
                },
                { title: 'PID', dataIndex: 'pid' },
                { title: '重启次数', dataIndex: 'restarts' },
                { title: '退出码', dataIndex: 'exit_code' },
                {
                  title: '操作',
                  render: (_, app) => (
                    <AppTerminal
                      agent={value}
                      instance={app.instance}
                      name={`${blueprint.name} / ${app.id}`}
                    />
                  ),
                },
              ]}
            />
            {blueprint.deployment_id && (
              <Link to={`/deployments/${blueprint.deployment_id}`}>当前部署</Link>
            )}
            {blueprint.result && (
              <Typography.Paragraph>
                最近任务：
                <Link to={`/deployments/${blueprint.result.id}`}>{blueprint.result.id}</Link>{' '}
                <StateTag state={blueprint.result.state} />
              </Typography.Paragraph>
            )}
          </Card>
        ))}
        {value.report.result && (
          <Typography.Paragraph className="block-gap">
            最近结果：
            <Link to={`/deployments/${value.report.result.id}`}>{value.report.result.id}</Link>{' '}
            <StateTag state={value.report.result.state} />
          </Typography.Paragraph>
        )}
      </Card>
      <BindingForm key={value.id} agent={value} />
      <Card title="部署记录" className="block-gap">
        <AgentJobs id={value.id} />
      </Card>
    </>
  );
}
function AgentJobs({ id }: { id: string }) {
  const jobs = useLoad<{ deployments: Job[] }>(
    `/v1/deployments?agent_id=${encodeURIComponent(id)}`,
    5000,
  );
  return (
    <>
      <ErrorBox error={jobs.error} />
      <JobTable jobs={jobs.data?.deployments ?? []} />
    </>
  );
}
function JobTable({ jobs, limit }: { jobs: Job[]; limit?: number }) {
  return (
    <Table
      rowKey="id"
      scroll={{ x: 700 }}
      pagination={limit ? false : { pageSize: 10 }}
      dataSource={[...jobs].sort((a, b) => b.created_at - a.created_at).slice(0, limit)}
      columns={[
        {
          title: '任务',
          dataIndex: 'id',
          render: (v: string) => <Link to={`/deployments/${v}`}>{v.slice(0, 8)}</Link>,
        },
        {
          title: '服务器',
          dataIndex: 'agent_id',
          render: (v: string) => <Link to={`/agents/${v}`}>{v.slice(0, 8)}</Link>,
        },
        { title: 'Blueprint', dataIndex: 'blueprint' },
        {
          title: '操作',
          dataIndex: 'action',
          render: (action) => (action === 'stop' ? '停止' : '部署'),
        },
        { title: '状态', dataIndex: 'state', render: (state) => <StateTag state={state} /> },
        { title: '创建时间', dataIndex: 'created_at', render: date },
      ]}
    />
  );
}
export function Deployments() {
  const [agentId, setAgentId] = useState<string>();
  const agents = useLoad<{ agents: Agent[] }>('/v1/agents');
  const jobs = useLoad<{ deployments: Job[] }>(
    `/v1/deployments${agentId ? `?agent_id=${encodeURIComponent(agentId)}` : ''}`,
    5000,
  );
  return (
    <>
      <Heading title="部署记录" subtitle="跟踪构建、下发、运行和回退结果。" />
      <ErrorBox error={jobs.error || agents.error} retry={jobs.refresh} />
      <Card>
        <Select
          aria-label="按服务器筛选"
          className="search"
          allowClear
          placeholder="全部服务器"
          value={agentId}
          onChange={setAgentId}
          options={agents.data?.agents.map((a) => ({ value: a.id, label: a.name }))}
        />
        <JobTable jobs={jobs.data?.deployments ?? []} />
      </Card>
    </>
  );
}
export function DeploymentDetail() {
  const { id } = useParams();
  const job = useLoad<Job>(`/v1/deployments/${encodeURIComponent(id!)}`, 5000, true);
  if (!job.data)
    return job.error ? <ErrorBox error={job.error} retry={job.refresh} /> : <Loading />;
  const value = job.data;
  return (
    <>
      <Heading
        title="部署详情"
        subtitle={value.id}
        actions={
          <Link to="/deployments">
            <Button>返回记录</Button>
          </Link>
        }
      />
      <ErrorBox error={job.error} retry={job.refresh} />
      {value.error && (
        <Alert className="block-gap" type="error" title="部署未完成" description={value.error} />
      )}
      <Card>
        <Descriptions
          column={{ xs: 1, md: 2 }}
          items={[
            { key: 'state', label: '状态', children: <StateTag state={value.state} /> },
            {
              key: 'agent',
              label: '服务器',
              children: <Link to={`/agents/${value.agent_id}`}>{value.agent_id}</Link>,
            },
            { key: 'blueprint', label: 'Blueprint', children: value.blueprint },
            { key: 'action', label: '操作', children: value.action === 'stop' ? '停止' : '部署' },
            {
              key: 'commit',
              label: 'commit',
              children: <Typography.Text code>{value.commit}</Typography.Text>,
            },
            { key: 'time', label: '创建时间', children: date(value.created_at) },
            { key: 'arch', label: '架构', children: value.plan?.architecture ?? '构建完成后显示' },
          ]}
        />
      </Card>
      <Card title="部署包" className="block-gap">
        {value.plan ? (
          <Table
            rowKey="id"
            scroll={{ x: 700 }}
            pagination={false}
            dataSource={value.plan.apps}
            columns={[
              { title: '实例', dataIndex: 'id' },
              {
                title: '大小',
                dataIndex: 'size',
                render: (v: number) => `${v.toLocaleString()} B`,
              },
              {
                title: 'SHA-256',
                dataIndex: 'sha256',
                render: (v) => (
                  <Typography.Text code copyable>
                    {v}
                  </Typography.Text>
                ),
              },
            ]}
          />
        ) : (
          <Empty description="尚未生成部署计划" />
        )}
      </Card>
    </>
  );
}
interface InitRequest {
  connection_mode?: 'agent_to_controller' | 'controller_to_agent';
  listen?: string;
  request_id: string;
  name: string;
  public_url: string;
  info: { architecture: string; hostname: string; os_release: string };
}
function decodeRequest(hash: string): InitRequest {
  if (!['http:', 'https:'].includes(location.protocol))
    throw new Error('请通过 HTTP 或 HTTPS 访问授权页面');
  const encoded = hash.slice(1);
  if (!encoded || encoded.length > 16384) throw new Error('请使用 pier-agent init 显示的授权链接');
  const value = JSON.parse(
    new TextDecoder('utf-8', { fatal: true }).decode(
      Uint8Array.from(atob(encoded.replace(/-/g, '+').replace(/_/g, '/')), (c) => c.charCodeAt(0)),
    ),
  ) as InitRequest;
  if (
    value.public_url !== location.origin ||
    !/^[a-f0-9]{64}$/.test(value.request_id) ||
    typeof value.name !== 'string' ||
    (value.connection_mode !== undefined &&
      !['agent_to_controller', 'controller_to_agent'].includes(value.connection_mode)) ||
    (value.connection_mode === 'controller_to_agent' && typeof value.listen !== 'string') ||
    !value.info ||
    !['amd64', 'arm64'].includes(value.info.architecture) ||
    typeof value.info.hostname !== 'string' ||
    typeof value.info.os_release !== 'string'
  )
    throw new Error('授权链接无效或与当前 controller 不匹配');
  return value;
}
export function Enrollment() {
  const location = useLocation();
  const [error, setError] = useState<Error>();
  const [pending, setPending] = useState(false);
  const [cancelled, setCancelled] = useState(false);
  const [pairing, setPairing] = useState('');
  const [endpoint, setEndpoint] = useState('');
  const [proxyMode, setProxyMode] = useState<ProxyMode>('keep');
  const [proxy, setProxy] = useState('');
  const [connectionError, setConnectionError] = useState<string | null>(null);
  const [grant, setGrant] = useState<{ id: string; expires_at: number }>();
  const [state, setState] = useState('');
  const [agentId, setAgentId] = useState<string>();
  const { message } = App.useApp();
  const decoded = useMemo(() => {
    try {
      return { request: decodeRequest(location.hash) };
    } catch (e) {
      return { error: e as Error };
    }
  }, [location.hash]);
  useEffect(() => {
    setPairing('');
    setEndpoint('');
    setProxyMode('keep');
    setProxy('');
    setConnectionError(null);
    setGrant(undefined);
    setState('');
    setAgentId(undefined);
    setCancelled(false);
    setError(undefined);
  }, [location.hash]);
  useEffect(() => {
    if (!grant) return;
    const controller = new AbortController();
    const timer = setInterval(() => {
      void api<{ state: string; agent_id: string | null; last_error?: string | null }>(
        `/v1/enrollments/${grant.id}`,
        'GET',
        undefined,
        true,
        controller.signal,
      )
        .then((value) => {
          if (controller.signal.aborted) return;
          setState(value.state);
          setConnectionError(value.last_error ?? null);
          if (value.agent_id) setAgentId(value.agent_id);
          if (['completed', 'expired'].includes(value.state)) {
            setPairing('');
            clearInterval(timer);
          }
        })
        .catch((e) => {
          if (!controller.signal.aborted) setError(e);
        });
    }, 2000);
    return () => {
      clearInterval(timer);
      controller.abort();
    };
  }, [grant]);
  const request = decoded.request;
  const authorizationLabel = pairing
    ? '更新代理并重试'
    : state === 'expired'
      ? '重新授权'
      : '授权接入';
  return (
    <>
      <Heading title="接入服务器" subtitle="核对服务器信息，授权后回到终端完成配对。" />
      <ErrorBox error={decoded.error || error} />
      {request && (
        <Card>
          <Descriptions
            column={1}
            items={[
              { key: 'name', label: '名称', children: request.name },
              {
                key: 'mode',
                label: '连接方式',
                children:
                  request.connection_mode === 'controller_to_agent'
                    ? 'Controller → Agent'
                    : 'Agent → Controller',
              },
              ...(request.listen
                ? [{ key: 'listen', label: 'Agent 本机监听', children: request.listen }]
                : []),
              { key: 'host', label: '主机', children: request.info.hostname },
              { key: 'arch', label: '架构', children: request.info.architecture },
              { key: 'os', label: '发行信息', children: <pre>{request.info.os_release}</pre> },
              { key: 'id', label: '请求', children: request.request_id },
            ]}
          />
          {request.connection_mode === 'controller_to_agent' &&
            !cancelled &&
            state !== 'completed' && (
              <Form layout="vertical">
                <Form.Item
                  label="Agent 可达地址"
                  required
                  extra="填写 Controller 能访问的域名/IP 和端口；使用 NAT 时填写映射后的地址。"
                >
                  <Input
                    aria-label="Agent 可达地址"
                    value={endpoint}
                    disabled={!!pairing || pending}
                    onChange={(e) => setEndpoint(e.target.value)}
                    placeholder="agent.example.com:7444"
                  />
                </Form.Item>
                <ConnectionProxy
                  mode={proxyMode}
                  setMode={setProxyMode}
                  value={proxy}
                  setValue={setProxy}
                  disabled={pending}
                />
              </Form>
            )}
          {connectionError && state !== 'completed' && (
            <Alert type="warning" title={connectionError} />
          )}
          {cancelled ? (
            <Alert type="info" title="已取消授权，可在终端按 Ctrl+C 结束初始化。" />
          ) : (
            <>
              {state && (
                <Typography.Paragraph>
                  <StateTag state={state} />
                  {state === 'completed' && agentId && (
                    <Link to={`/agents/${agentId}`}>查看服务器</Link>
                  )}
                </Typography.Paragraph>
              )}
              {pairing && (
                <>
                  <Alert
                    type="success"
                    title="已授权，请将一次性配对凭据粘贴回终端"
                    description={`有效期至 ${date(grant?.expires_at ?? null)}`}
                  />
                  <Input.TextArea
                    aria-label="一次性配对凭据"
                    className="block-gap"
                    readOnly
                    value={pairing}
                    rows={5}
                  />
                  <Button
                    onClick={async () => {
                      try {
                        await navigator.clipboard.writeText(pairing);
                        void message.success('已复制，请粘贴回终端');
                      } catch {
                        void message.info('请手动选中并复制配对凭据');
                      }
                    }}
                  >
                    复制配对凭据
                  </Button>
                </>
              )}
              {(!pairing || request.connection_mode === 'controller_to_agent') &&
                state !== 'completed' && (
                  <Space>
                    <Button
                      type="primary"
                      aria-label={authorizationLabel}
                      aria-busy={pending}
                      loading={pending}
                      disabled={
                        pending ||
                        (request.connection_mode === 'controller_to_agent' &&
                          (!endpoint.trim() || (proxyMode === 'set' && !proxy)))
                      }
                      onClick={async () => {
                        setPending(true);
                        setError(undefined);
                        try {
                          const value = await api<{
                            id: string;
                            pairing: string;
                            expires_at: number;
                          }>('/v1/enrollments', 'POST', {
                            ...request,
                            ...(request.connection_mode === 'controller_to_agent'
                              ? {
                                  agent_endpoint: endpoint.trim(),
                                  agent_proxy: proxyValue(proxyMode, proxy),
                                }
                              : {}),
                          });
                          setPairing(value.pairing);
                          setGrant({ id: value.id, expires_at: value.expires_at });
                          setState('authorized');
                          setProxy('');
                          setProxyMode('keep');
                          setConnectionError(null);
                        } catch (e) {
                          setError(e as Error);
                        } finally {
                          setPending(false);
                        }
                      }}
                    >
                      {authorizationLabel}
                    </Button>
                    <Button onClick={() => setCancelled(true)}>取消</Button>
                  </Space>
                )}
            </>
          )}
        </Card>
      )}
    </>
  );
}
export function Settings({ onChanged }: { onChanged: () => void }) {
  const [pending, setPending] = useState(false);
  const [error, setError] = useState<Error>();
  return (
    <>
      <Heading title="账号设置" subtitle="修改密码后，所有已登录会话都会失效。" />
      <Card className="settings-card">
        <ErrorBox error={error} />
        <Form
          layout="vertical"
          onFinish={async (values) => {
            setPending(true);
            setError(undefined);
            try {
              await api('/v1/auth/password', 'POST', {
                current_password: values.current,
                new_password: values.password,
              });
              onChanged();
            } catch (e) {
              setError(e as Error);
            } finally {
              setPending(false);
            }
          }}
        >
          <Form.Item name="current" label="当前密码" rules={[{ required: true }]}>
            <Input.Password autoComplete="current-password" />
          </Form.Item>
          <Form.Item
            name="password"
            label="新密码"
            rules={[
              { required: true },
              {
                validator: (_, v: string) =>
                  v && [...v].length >= 12 && new TextEncoder().encode(v).length <= 1024
                    ? Promise.resolve()
                    : Promise.reject(new Error('密码至少 12 个字符，最多 1024 字节')),
              },
            ]}
          >
            <Input.Password autoComplete="new-password" />
          </Form.Item>
          <Form.Item
            name="confirm"
            label="确认新密码"
            dependencies={['password']}
            rules={[
              { required: true },
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
          <Button type="primary" htmlType="submit" loading={pending}>
            修改密码
          </Button>
        </Form>
      </Card>
    </>
  );
}
