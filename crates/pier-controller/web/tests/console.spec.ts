import { test, expect, type Page, type WebSocketRoute } from '@playwright/test';
import { readFileSync } from 'node:fs';
const password = 'browser-password-123';
async function login(page: Page, value = password) {
  await page.goto('/login');
  await page.getByLabel('用户名', { exact: true }).fill('admin');
  await page.getByLabel('密码', { exact: true }).fill(value);
  await page.getByRole('button', { name: '登录', exact: true }).click();
  await expect(page.getByRole('button', { name: '退出', exact: true })).toBeVisible();
}
test.describe.serial('controller console', () => {
  let violations: string[] = [];
  test.beforeEach(async ({ page }) => {
    violations = [];
    page.on('console', (m) => {
      if (m.type() === 'error' && /Content Security Policy|violates/.test(m.text()))
        violations.push(m.text());
    });
    page.on('pageerror', (error) => violations.push(error.message));
  });
  test.afterEach(() => {
    expect(violations).toEqual([]);
  });
  test('first initialization, protected cookie, CSRF and logout', async ({
    page,
    context,
  }, testInfo) => {
    await page.goto('/');
    await expect(page).toHaveURL('/init');
    await page.getByLabel('用户名', { exact: true }).fill('admin');
    await page.getByLabel('密码', { exact: true }).fill(password);
    await page.getByLabel('确认密码', { exact: true }).fill(password);
    await page
      .getByLabel('仓库地址', { exact: true })
      .fill(
        readFileSync(new URL('../../../../target/web-e2e-repository.txt', import.meta.url), 'utf8'),
      );
    await page.getByRole('button', { name: '运行设置', exact: true }).click();
    await page.getByLabel('Agent 通信监听地址', { exact: true }).fill('127.0.0.1:17444');
    await expect(page.getByLabel('Web 公开地址', { exact: true })).toHaveValue(
      test.info().project.use.baseURL!,
    );
    await page.getByLabel('构建并发数', { exact: true }).fill('3');
    await expect(page.getByLabel('仓库同步与构建代理', { exact: true })).toBeVisible();
    await page.getByRole('button', { name: '创建管理员' }).click();
    await expect(page.getByRole('heading', { name: '运行概览' })).toBeVisible();
    const beforeSync = await (await page.request.get('/v1/repository')).json();
    expect(beforeSync.commit).toBeNull();
    expect(beforeSync.needs_sync).toBe(true);
    const runtime = await (await page.request.get('/v1/settings')).json();
    expect(runtime.active.tcp_listen).toBe('127.0.0.1:17444');
    expect(runtime.active.agent_endpoint).toBe(`${new URL(page.url()).hostname}:17444`);
    expect(runtime.active.max_concurrent_builds).toBe(3);
    expect(runtime.agent_listener.listening).toBe(true);
    expect(runtime.restart_required).toBe(false);
    const secure = new URL(page.url()).protocol === 'https:';
    expect(await page.evaluate(() => window.isSecureContext)).toBe(secure);
    const cookie = (await context.cookies()).find(
      (c) => c.name === (secure ? '__Host-pier_session' : 'pier_session'),
    )!;
    expect(cookie.httpOnly).toBe(true);
    expect(cookie.secure).toBe(secure);
    expect(cookie.sameSite).toBe('Strict');
    expect(await page.evaluate(() => document.cookie)).not.toContain('pier_session');
    expect(await page.evaluate(() => Object.keys(localStorage))).toEqual([]);
    expect(
      (
        await page.request.post('/v1/agents', {
          headers: { Origin: test.info().project.use.baseURL! },
          data: { name: 'blocked' },
        })
      ).status(),
    ).toBe(403);
    await page.goto('/init');
    await expect(page.getByRole('heading', { name: '运行概览' })).toBeVisible();
    await page.screenshot({ path: testInfo.outputPath('overview.png'), fullPage: true });
    await page.getByRole('button', { name: '退出', exact: true }).click();
    await expect(page.getByRole('button', { name: '登录', exact: true })).toBeVisible();
    expect((await page.request.get('/v1/agents')).status()).toBe(401);
  });
  test('runtime edits stay pending until restart and can be reverted', async ({ page }) => {
    await login(page);
    await page.getByRole('link', { name: '控制器设置', exact: true }).click();
    await expect(page.getByLabel('构建并发数', { exact: true })).toHaveValue('3');
    await page.getByLabel('构建并发数', { exact: true }).fill('4');
    await page.getByLabel('仓库同步与构建代理', { exact: true }).click();
    await page.getByText('设置代理', { exact: true }).click();
    await page.getByLabel('HTTPS 代理', { exact: true }).fill('http://proxy.example:7890');
    await page.getByRole('button', { name: '保存运行设置', exact: true }).click();
    await expect(page.getByText('已保存，重启后生效', { exact: true })).toBeVisible();
    const saved = await (await page.request.get('/v1/settings')).json();
    expect(saved.active.max_concurrent_builds).toBe(3);
    expect(saved.saved.max_concurrent_builds).toBe(4);
    expect(saved.active.build_proxy.https_proxy).toBeNull();
    expect(saved.saved.build_proxy.https_proxy).toBe('http://proxy.example:7890');
    expect(saved.restart_required).toBe(true);
    await page.reload();
    await expect(page.getByLabel('构建并发数', { exact: true })).toHaveValue('4');
    await page.getByLabel('构建并发数', { exact: true }).fill('3');
    await page.getByLabel('仓库同步与构建代理', { exact: true }).click();
    await page.getByText('清除代理', { exact: true }).click();
    await page.getByRole('button', { name: '保存运行设置', exact: true }).click();
    await expect(page.getByText('已保存，重启后生效', { exact: true })).toHaveCount(0);
    await page.getByRole('link', { name: '定义仓库', exact: true }).click();
    await expect(
      page.getByText('，未配置对应代理时直连。保存或清除代理后需重启生效。', { exact: false }),
    ).toBeVisible();
    await page.getByRole('link', { name: '控制器设置中的代理', exact: true }).click();
    await expect(page).toHaveURL('/settings/controller');
  });
  test('agent upgrade status exposes versions and blocks deployment during restart', async ({
    page,
  }) => {
    await login(page);
    let phase = 'restarting';
    await page.route('**/v1/agents/update-test', async (route) =>
      route.fulfill({
        json: {
          id: 'update-test',
          name: 'upgrade-agent',
          online: true,
          last_seen: null,
          info: { hostname: 'upgrade-host', architecture: 'amd64', os_release: 'Ubuntu 24.04' },
          report: { apps: [], result: null, deployment_id: null },
          software: {
            version: '0.1.0',
            package: { version: '0.1.0', revision: 1 },
            supported: true,
            reason: null,
          },
          upgrade: {
            target: { package: { version: '0.2.0', revision: 2 } },
            reason: null,
            status: {
              phase,
              error: phase === 'failed' ? 'installation failed; manual recovery required' : null,
            },
          },
        },
      }),
    );
    await page.goto('/agents/update-test');
    await expect(page.getByText('0.1.0-1', { exact: true })).toBeVisible();
    await expect(page.getByText('0.2.0-2', { exact: true })).toBeVisible();
    await expect(page.getByText('重启中', { exact: true })).toBeVisible();
    await expect(page.getByRole('button', { name: '创建部署', exact: true })).toBeDisabled();
    phase = 'failed';
    await page.reload();
    await expect(page.getByText('升级失败，需手动恢复', { exact: true })).toBeVisible();
    await expect(
      page.getByText('installation failed; manual recovery required', { exact: true }),
    ).toBeVisible();
    await expect(page.getByRole('button', { name: '创建部署', exact: true })).toBeEnabled();
  });
  test('app terminal capability, input, resize, CSP and session disposal', async ({ page }) => {
    await login(page);
    let online = false;
    let supported = false;
    let active: WebSocketRoute | undefined;
    let opened = 0;
    let closed = 0;
    const controls: { type: string; bytes?: number }[] = [];
    const input: Buffer[] = [];
    await page.route('**/v1/agents/terminal-test', (route) =>
      route.fulfill({
        json: {
          id: 'terminal-test',
          name: 'terminal-server',
          online,
          last_seen: null,
          info: { hostname: 'terminal-host', architecture: 'amd64', os_release: 'Ubuntu 24.04' },
          report: {
            capabilities: supported ? ['app_terminal_v1'] : [],
            deployment_id: null,
            result: null,
            apps: [
              {
                id: 'demo',
                instance: 'demo-instance',
                state: 'running',
                pid: 123,
                restarts: 0,
                exit_code: null,
              },
            ],
          },
        },
      }),
    );
    await page.route('**/v1/agents/terminal-test/apps/demo-instance/terminals', (route) => {
      expect(route.request().headers()['x-csrf-token']).toBeTruthy();
      expect(route.request().postDataJSON().cols).toBeGreaterThan(0);
      return route.fulfill({ json: { websocket_url: `/v1/terminals/terminal-${opened}/ws` } });
    });
    await page.routeWebSocket('**/v1/terminals/*/ws', (socket) => {
      active = socket;
      opened++;
      socket.onClose(() => closed++);
      socket.onMessage((message) => {
        if (typeof message === 'string') controls.push(JSON.parse(message));
        else input.push(message);
      });
      socket.send(
        JSON.stringify({
          type: 'ready',
          user: 'pier_demo',
          home: '/var/lib/pier-agent/apps/demo/data',
        }),
      );
      socket.send(Buffer.from('hello terminal\r\n'));
    });
    await page.goto('/agents/terminal-test');
    await expect(page.getByRole('button', { name: '终端', exact: true })).toBeDisabled();
    online = true;
    await page.reload();
    await expect(page.getByRole('button', { name: '终端', exact: true })).toBeDisabled();
    supported = true;
    await page.reload();
    await page.getByRole('button', { name: '终端', exact: true }).click();
    await expect(page.getByText('已连接', { exact: true })).toBeVisible();
    await expect(page.getByText('pier_demo · /var/lib/pier-agent/apps/demo/data')).toBeVisible();
    await page.locator('.xterm-helper-textarea').pressSequentially('echo test');
    await page.locator('.xterm-helper-textarea').press('Enter');
    await expect.poll(() => Buffer.concat(input).toString()).toContain('echo test\r');
    await page.getByRole('button', { name: '全屏', exact: true }).click();
    await expect.poll(() => controls.some((c) => c.type === 'resize')).toBe(true);
    await expect.poll(() => controls.some((c) => c.type === 'ack' && c.bytes === 16)).toBe(true);
    active!.send(JSON.stringify({ type: 'exit', reason: 'deployment_started' }));
    active!.close();
    await expect(page.getByText('开始重新部署，终端已关闭')).toBeVisible();
    await page.getByRole('button', { name: '重新连接', exact: true }).click();
    await expect.poll(() => opened).toBe(2);
    await expect(page.getByText('已连接', { exact: true })).toBeVisible();
    const before = closed;
    await page
      .getByRole('dialog')
      .getByRole('button', { name: /close|关闭/i })
      .click();
    await expect.poll(() => closed).toBeGreaterThan(before);
    await expect(page.locator('.app-terminal')).toHaveCount(0);
  });
  test('passive enrollment SOCKS5 retry and connection proxy editing', async ({ page }) => {
    await login(page);
    const request = {
      request_id: 'ab'.repeat(32),
      name: 'passive-browser',
      public_url: new URL(page.url()).origin,
      connection_mode: 'controller_to_agent',
      listen: '0.0.0.0:7444',
      info: { architecture: 'amd64', hostname: 'passive-browser', os_release: 'test' },
    };
    let enrollment: Record<string, unknown> | undefined;
    await page.route('**/v1/enrollments', async (route) => {
      enrollment = route.request().postDataJSON();
      await route.fulfill({
        json: {
          id: request.request_id,
          pairing: 'pier-pair-v2.test',
          expires_at: Math.floor(Date.now() / 1000) + 600,
        },
      });
    });
    await page.route(`**/v1/enrollments/${request.request_id}`, (route) =>
      route.fulfill({ json: { state: 'authorized', agent_id: null } }),
    );
    await page.goto(`/agent/init#${Buffer.from(JSON.stringify(request)).toString('base64url')}`);
    await expect(page.getByText('Controller → Agent', { exact: true })).toBeVisible();
    await expect(page.getByRole('button', { name: '授权接入', exact: true })).toBeDisabled();
    await page.getByLabel('Agent 可达地址', { exact: true }).fill('agent.example.test:7444');
    await page.getByRole('combobox', { name: '连接代理' }).click();
    await page.getByTitle('替换代理', { exact: true }).click();
    await expect(page.getByRole('button', { name: '授权接入', exact: true })).toBeDisabled();
    await page
      .getByLabel('SOCKS5 代理地址', { exact: true })
      .fill('socks5://user:password@proxy.test:1080');
    await expect(page.getByLabel('SOCKS5 代理地址', { exact: true })).toHaveAttribute(
      'type',
      'password',
    );
    await page.getByRole('button', { name: '授权接入', exact: true }).click();
    await expect(page.getByLabel('一次性配对凭据')).toHaveValue('pier-pair-v2.test');
    expect(enrollment).toMatchObject({
      connection_mode: 'controller_to_agent',
      agent_endpoint: 'agent.example.test:7444',
      agent_proxy: 'socks5://user:password@proxy.test:1080',
    });
    await expect(page.getByLabel('SOCKS5 代理地址', { exact: true })).toHaveCount(0);
    await expect(page.getByLabel('Agent 可达地址', { exact: true })).toBeDisabled();
    await expect(page.getByRole('button', { name: '更新代理并重试', exact: true })).toHaveAttribute(
      'aria-busy',
      'false',
    );
    await page.getByRole('button', { name: '更新代理并重试', exact: true }).click();
    await expect.poll(() => enrollment).not.toHaveProperty('agent_proxy');
    await page.getByRole('combobox', { name: '连接代理' }).click();
    await page.getByTitle('替换代理', { exact: true }).click();
    await page
      .getByLabel('SOCKS5 代理地址', { exact: true })
      .fill('socks5://fixed:secret@proxy.test:1080');
    await page.getByRole('button', { name: '更新代理并重试', exact: true }).click();
    await expect.poll(() => enrollment?.agent_proxy).toBe('socks5://fixed:secret@proxy.test:1080');
    await expect(page.getByLabel('一次性配对凭据')).toHaveValue('pier-pair-v2.test');
    let proxyConfigured = true;
    let connectionPatch: Record<string, unknown> | undefined;
    let endpoint = 'agent.example.test:7444';
    let busy = true;
    await page.route('**/v1/agents/passive-browser', (route) =>
      route.fulfill({
        json: {
          id: 'passive-browser',
          name: 'passive-browser',
          online: false,
          info: null,
          last_seen: 1,
          connection: {
            mode: 'controller_to_agent',
            endpoint,
            proxy_configured: proxyConfigured,
            state: 'reconnecting',
            last_error: '连接失败，正在重试',
          },
          software: null,
          upgrade: { target: null, status: null, reason: null },
          report: { apps: [], deployment_id: null, result: null },
        },
      }),
    );
    await page.route('**/v1/agents/passive-browser/binding', (route) =>
      route.fulfill({ status: 404, json: { error: 'resource not found' } }),
    );
    await page.route('**/v1/agents/passive-browser/connection', (route) => {
      if (busy)
        return route.fulfill({ status: 409, json: { error: 'agent is deploying or upgrading' } });
      connectionPatch = route.request().postDataJSON();
      endpoint = connectionPatch!.endpoint as string;
      if (connectionPatch!.proxy !== undefined) proxyConfigured = connectionPatch!.proxy !== null;
      return route.fulfill({ json: {} });
    });
    await page.goto('/agents/passive-browser');
    await expect(page.getByText('连接失败，正在重试')).toBeVisible();
    await page.getByLabel('Agent 可达地址', { exact: true }).fill('next.example.test:7444');
    await page.getByRole('button', { name: '保存并重连' }).click();
    await expect(page.getByText('agent is deploying or upgrading')).toBeVisible();
    busy = false;
    await page.getByRole('button', { name: '保存并重连' }).click();
    await expect.poll(() => endpoint).toBe('next.example.test:7444');
    expect(connectionPatch).not.toHaveProperty('proxy');
    await expect(page.getByText('连接代理：已配置 SOCKS5')).toBeVisible();
    await page.getByRole('combobox', { name: '连接代理' }).click();
    await page.getByTitle('不使用代理', { exact: true }).click();
    await page.getByRole('button', { name: '保存并重连' }).click();
    await expect.poll(() => connectionPatch?.proxy).toBeNull();
    await expect(page.getByText('连接代理：直连')).toBeVisible();
    await page.getByRole('combobox', { name: '连接代理' }).click();
    await page.getByTitle('替换代理', { exact: true }).click();
    await page
      .getByLabel('SOCKS5 代理地址', { exact: true })
      .fill('socks5://new:secret@new-proxy.test:1080');
    await page.getByRole('button', { name: '保存并重连' }).click();
    await expect.poll(() => connectionPatch?.proxy).toBe('socks5://new:secret@new-proxy.test:1080');
    await expect(page.getByLabel('SOCKS5 代理地址', { exact: true })).toHaveCount(0);
    await expect(page.getByText('连接代理：已配置 SOCKS5')).toBeVisible();
  });
  test('repository, declarations, binding edits and deployment flow', async ({
    page,
  }, testInfo) => {
    await login(page);
    await page.getByRole('link', { name: '定义仓库', exact: true }).click();
    await page.getByRole('button', { name: '立即同步' }).click();
    await expect(page.getByText('仓库同步完成')).toBeVisible();
    const initial = await (await page.request.get('/v1/repository')).json();
    await page.getByLabel('分支或引用', { exact: true }).fill('missing-branch');
    await page.getByRole('button', { name: '保存配置', exact: true }).click();
    await expect(page.getByText('仓库配置已保存，请手动同步')).toBeVisible();
    await expect(page.getByText('待同步', { exact: true })).toBeVisible();
    const saved = await (await page.request.get('/v1/repository')).json();
    expect(saved.commit).toBe(initial.commit);
    expect(saved.error).toBeNull();
    await page.getByRole('button', { name: '立即同步' }).click();
    await expect(
      page
        .getByText('repository sync or catalog validation failed; previous catalog retained')
        .first(),
    ).toBeVisible();
    await page.getByLabel('分支或引用', { exact: true }).fill('main');
    await page.getByRole('button', { name: '保存配置', exact: true }).click();
    await expect(page.getByText('待同步', { exact: true })).toHaveCount(0);

    await page.getByRole('link', { name: '应用', exact: true }).click();
    await page.getByRole('button', { name: 'demo', exact: true }).click();
    await expect(page.getByRole('dialog')).toContainText('SECRET');
    await page.getByRole('button', { name: '关闭', exact: true }).click();
    await page.getByRole('link', { name: 'Blueprint', exact: true }).click();
    await page.getByRole('button', { name: 'web-server', exact: true }).click();
    await expect(page.getByRole('dialog')).toContainText('应用全部变量');
    await page.getByRole('button', { name: '关闭', exact: true }).click();
    await page.getByRole('link', { name: '服务器', exact: true }).click();
    await page.getByRole('button', { name: '手动注册' }).click();
    await page.getByLabel('服务器名称').fill('browser-agent');
    await page.getByRole('button', { name: '注册', exact: true }).click();
    await expect(page.getByRole('dialog')).toContainText('token');
    await page.getByRole('button', { name: '关闭', exact: true }).click();
    await page.getByRole('link', { name: 'browser-agent', exact: true }).click();
    const id = page.url().split('/').pop()!;
    await expect(page.getByRole('button', { name: '创建部署' })).toBeDisabled();
    await page.getByRole('combobox', { name: '选择 Blueprint' }).click();
    await page.getByText('web-server · blueprints/web', { exact: true }).click();
    await page.getByLabel('SECRET 的值', { exact: true }).fill('keep-this-private');
    await page.getByRole('button', { name: '保存绑定' }).click();
    await expect(page.getByText('绑定已保存，创建部署后应用到服务器')).toBeVisible();
    await expect(page.getByText('保留已保存值', { exact: true })).toBeVisible();
    await page.getByRole('combobox', { name: 'PORT 的填写方式' }).click();
    await page.getByTitle('设置值', { exact: true }).click();
    await page.getByLabel('PORT 的值', { exact: true }).fill('9090');
    const patchPromise = page.waitForRequest((r) => r.method() === 'PATCH');
    await page.getByRole('button', { name: '保存绑定' }).click();
    const patch = await patchPromise;
    expect(patch.postDataJSON().variables).toEqual({ PORT: '9090' });
    const binding = await (await page.request.get(`/v1/agents/${id}/binding`)).json();
    expect(binding.variable_names).toEqual(['PORT', 'SECRET']);
    expect(JSON.stringify(binding)).not.toContain('keep-this-private');
    // Real binding/API above; emulate an online agent and job here. Rust Docker
    // lifecycle tests cover the real encrypted agent deployment independently.
    await page.route(`**/v1/agents/${id}`, async (route) => {
      const response = await route.fetch();
      const body = await response.json();
      await route.fulfill({
        json: {
          ...body,
          online: true,
          info: { hostname: 'fixture', architecture: 'amd64', os_release: 'fixture' },
        },
      });
    });
    const jobId = 'browser-job';
    let polls = 0;
    let submissions = 0;
    let submitted: Record<string, unknown> | undefined;
    await page.route('**/v1/deployments', async (route) => {
      if (route.request().method() === 'POST') {
        submissions++;
        if (submissions === 1) {
          await route.fulfill({
            status: 409,
            json: { error: 'catalog changed; reload before deploying' },
          });
          return;
        }
        submitted = route.request().postDataJSON();
        await route.fulfill({ json: { id: jobId, state: 'building' } });
      } else await route.continue();
    });
    await page.route(`**/v1/deployments/${jobId}`, async (route) => {
      polls++;
      await route.fulfill({
        json: {
          id: jobId,
          agent_id: id,
          blueprint: 'blueprints/web',
          commit: 'fixture-commit',
          state: polls > 1 ? 'succeeded' : 'building',
          error: null,
          created_at: 1790467200,
          plan: null,
        },
      });
    });
    await page.reload();
    await page.getByRole('button', { name: '创建部署' }).click();
    await page.getByLabel('api 的构建镜像').fill('pier-builder-rust:almalinux8');
    await page.getByRole('button', { name: '确认部署' }).click();
    await expect(
      page.getByText('catalog changed; reload before deploying', { exact: true }),
    ).toBeVisible();
    await expect(page.getByRole('button', { name: '确认部署' })).toBeEnabled();
    expect(submissions).toBe(1);
    await page.getByRole('button', { name: '确认部署' }).click();
    await expect(page.getByRole('heading', { name: '部署详情' })).toBeVisible();
    await expect(page.getByText('成功', { exact: true })).toBeVisible({ timeout: 15000 });
    expect(submitted?.images).toEqual({ api: 'pier-builder-rust:almalinux8' });
    expect(submitted?.agent_id).toBe(id);
    expect(submitted?.commit).toMatch(/^[0-9a-f]{40}$/);
    await page.screenshot({ path: testInfo.outputPath('deployment.png'), fullPage: true });
  });
  test('agent fragment survives login, pairing copy, invalid link and password revocation', async ({
    page,
    context,
  }) => {
    const request = {
      request_id: 'a'.repeat(64),
      name: 'enroll-browser',
      public_url: test.info().project.use.baseURL!,
      info: { hostname: 'test-host', architecture: 'amd64', os_release: 'test-system' },
    };
    const hash = Buffer.from(JSON.stringify(request)).toString('base64url');
    await page.goto(`/agent/init#${hash}`);
    await page.getByLabel('用户名', { exact: true }).fill('admin');
    await page.getByLabel('密码', { exact: true }).fill('incorrect-password');
    await page.getByRole('button', { name: '登录', exact: true }).click();
    await expect(page.getByRole('alert')).toContainText('authentication required');
    expect(new URL(page.url()).hash).toBe(`#${hash}`);
    await page.getByLabel('密码', { exact: true }).fill(password);
    await page.getByRole('button', { name: '登录', exact: true }).click();
    await expect(page.getByRole('heading', { name: '接入服务器' })).toBeVisible();
    expect(new URL(page.url()).hash).toBe(`#${hash}`);
    await expect(page.getByRole('button', { name: '授权接入' })).toBeVisible();
    await page.getByRole('button', { name: '授权接入' }).click();
    await expect(page.getByLabel('一次性配对凭据')).toHaveValue(/^pier-pair-v2\./);
    if (new URL(page.url()).protocol === 'https:') {
      await context.grantPermissions(['clipboard-read', 'clipboard-write']);
      await page.getByRole('button', { name: '复制配对凭据' }).click();
      expect(await page.evaluate(() => navigator.clipboard.readText())).toMatch(/^pier-pair-v2\./);
    } else {
      expect(await page.evaluate(() => navigator.clipboard)).toBeUndefined();
      await page.getByRole('button', { name: '复制配对凭据' }).click();
      await expect(page.getByText('请手动选中并复制配对凭据')).toBeVisible();
      await expect(page.getByLabel('一次性配对凭据')).toHaveValue(/^pier-pair-v2\./);
    }
    await page.goto('/agent/init#invalid');
    await expect(page.getByRole('button', { name: '授权接入' })).toHaveCount(0);
    await page.goto('/settings');
    await page.getByLabel('当前密码', { exact: true }).fill(password);
    await page.getByLabel('新密码', { exact: true }).fill('updated-password-456');
    await page.getByLabel('确认新密码', { exact: true }).fill('updated-password-456');
    await page.getByRole('button', { name: '修改密码' }).click();
    await expect(page.getByRole('button', { name: '登录', exact: true })).toBeVisible();
    expect((await page.request.get('/v1/agents')).status()).toBe(401);
    await login(page, 'updated-password-456');
  });
  test('mobile login and expired session return to authentication', async ({
    page,
    context,
  }, testInfo) => {
    await page.setViewportSize({ width: 390, height: 844 });
    await page.goto('/login');
    await expect(page.getByRole('button', { name: '登录', exact: true })).toBeVisible();
    expect(await page.evaluate(() => document.documentElement.scrollWidth)).toBeLessThanOrEqual(
      390,
    );
    await page.screenshot({ path: testInfo.outputPath('mobile-login.png'), fullPage: true });
    await login(page, 'updated-password-456');
    await context.clearCookies();
    // A fresh protected request after session loss must unmount private pages.
    await page.evaluate(() => {
      history.pushState(null, '', '/repository');
      window.dispatchEvent(new PopStateEvent('popstate'));
    });
    await expect(page.getByRole('button', { name: '登录', exact: true })).toBeVisible();
    await expect(page.getByRole('heading', { name: '定义仓库' })).toHaveCount(0);
  });
});
