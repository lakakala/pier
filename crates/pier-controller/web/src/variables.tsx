import { useEffect, useState } from 'react';
import { Link } from 'react-router-dom';
import { App, Button, Form, Input, Modal, Popconfirm, Space, Table, Typography } from 'antd';
import { api, type GlobalVariable } from './api';
import { ErrorBox, Heading, useLoad } from './components';

export function GlobalVariables() {
  const variables = useLoad<{ variables: GlobalVariable[] }>('/v1/variables');
  const [editor, setEditor] = useState<GlobalVariable | null>();
  const [error, setError] = useState<Error>();
  const [editError, setEditError] = useState<Error>();
  const [pending, setPending] = useState(false);
  const [deleting, setDeleting] = useState<string>();
  const [form] = Form.useForm();
  const { message } = App.useApp();
  useEffect(() => {
    if (editor === undefined) return;
    form.resetFields();
    form.setFieldsValue({ name: editor?.name ?? '', value: editor?.value ?? '' });
    setEditError(undefined);
  }, [editor, form]);
  return (
    <>
      <Heading
        title="全局变量"
        subtitle="所有服务器和 Blueprint 均可引用；修改后在下次创建部署时生效。"
        actions={
          <>
            <Button onClick={variables.refresh}>刷新</Button>
            <Button type="primary" onClick={() => setEditor(null)}>
              新增变量
            </Button>
          </>
        }
      />
      <ErrorBox error={error || variables.error} retry={variables.refresh} />
      <Table<GlobalVariable>
        rowKey="name"
        loading={!variables.data && !variables.error}
        dataSource={variables.data?.variables}
        scroll={{ x: 760 }}
        columns={[
          { title: '名称', dataIndex: 'name', width: 190 },
          {
            title: '值',
            dataIndex: 'value',
            width: 300,
            render: (value: string) => (
              <Typography.Paragraph
                style={{
                  whiteSpace: 'pre-wrap',
                  overflowWrap: 'anywhere',
                  maxHeight: 160,
                  overflow: 'auto',
                  marginBottom: 0,
                }}
                copyable={{ text: value }}
              >
                {value === '' ? (
                  <Typography.Text type="secondary">空字符串</Typography.Text>
                ) : (
                  value
                )}
              </Typography.Paragraph>
            ),
          },
          {
            title: '引用位置',
            render: (_, variable) =>
              variable.references.length ? (
                <Space orientation="vertical">
                  {variable.references.map((ref) => (
                    <div key={`${ref.agent_id}/${ref.blueprint}/${ref.variable}`}>
                      <Link to={`/agents/${ref.agent_id}`}>{ref.agent_name || ref.agent_id}</Link>
                      <div>
                        {ref.blueprint} · {ref.variable}
                      </div>
                    </div>
                  ))}
                </Space>
              ) : (
                <Typography.Text type="secondary">未被引用</Typography.Text>
              ),
          },
          {
            title: '操作',
            width: 170,
            render: (_, variable) => (
              <Space>
                <Button onClick={() => setEditor(variable)}>编辑</Button>
                <Popconfirm
                  title={`删除变量 ${variable.name}？`}
                  disabled={variable.references.length > 0}
                  onConfirm={async () => {
                    setDeleting(variable.name);
                    setError(undefined);
                    try {
                      await api(`/v1/variables/${encodeURIComponent(variable.name)}`, 'DELETE');
                      void message.success('变量已删除');
                    } catch (e) {
                      setError(e as Error);
                    } finally {
                      setDeleting(undefined);
                      variables.refresh();
                    }
                  }}
                >
                  <Button
                    danger
                    disabled={variable.references.length > 0}
                    loading={deleting === variable.name}
                    title={variable.references.length ? '请先解除这些绑定中的引用' : undefined}
                  >
                    删除
                  </Button>
                </Popconfirm>
              </Space>
            ),
          },
        ]}
      />
      <Modal
        title={editor ? `编辑变量 ${editor.name}` : '新增变量'}
        open={editor !== undefined}
        onCancel={() => {
          if (!pending) setEditor(undefined);
        }}
        footer={null}
        forceRender
      >
        <ErrorBox error={editError} />
        <Form
          form={form}
          layout="vertical"
          onFinish={async (values: { name: string; value: string }) => {
            setPending(true);
            setEditError(undefined);
            try {
              await api(
                editor ? `/v1/variables/${encodeURIComponent(editor.name)}` : '/v1/variables',
                editor ? 'PUT' : 'POST',
                editor ? { value: values.value } : values,
              );
              setEditor(undefined);
              setError(undefined);
              variables.refresh();
              void message.success('变量已保存，下次创建部署时生效');
            } catch (e) {
              setEditError(e as Error);
            } finally {
              setPending(false);
            }
          }}
        >
          <Form.Item
            name="name"
            label="变量名称"
            rules={[
              { required: true, message: '请输入变量名称' },
              {
                pattern: /^[A-Za-z_][A-Za-z0-9_]*$/,
                message: '名称只能包含字母、数字和下划线，不能以数字开头',
              },
              {
                validator: (_, value) =>
                  value === 'PIER_ARCH'
                    ? Promise.reject(new Error('PIER_ARCH 是保留名称'))
                    : Promise.resolve(),
              },
            ]}
          >
            <Input disabled={!!editor || pending} autoComplete="off" />
          </Form.Item>
          <Form.Item name="value" label="变量值" extra="允许空字符串和多行文本，保存后可查看。">
            <Input.TextArea rows={5} autoComplete="off" disabled={pending} />
          </Form.Item>
          <Button
            type="primary"
            htmlType="submit"
            aria-label="保存变量"
            loading={pending}
            disabled={pending}
          >
            保存变量
          </Button>
        </Form>
      </Modal>
    </>
  );
}
