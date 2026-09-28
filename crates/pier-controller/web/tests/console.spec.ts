import { test, expect, type Page } from '@playwright/test';
import { readFileSync } from 'node:fs';
const password = 'browser-password-123';
async function login(page: Page, value = password) {
  await page.goto('/login');
  await page.getByLabel('用户名', { exact: true }).fill('admin');
  await page.getByLabel('密码', { exact: true }).fill(value);
  await page.getByRole('button', { name: '登录', exact: true }).click();
  await expect(page.getByRole('button', { name: '退出', exact: true })).toBeVisible();
}
test.describe.serial('controller console over HTTPS', () => {
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
      'https://localhost:8444',
    );
    await page.getByLabel('构建并发数', { exact: true }).fill('3');
    await page.getByRole('button', { name: '创建管理员' }).click();
    await expect(page.getByRole('heading', { name: '运行概览' })).toBeVisible();
    const beforeSync = await (await page.request.get('/v1/repository')).json();
    expect(beforeSync.commit).toBeNull();
    expect(beforeSync.needs_sync).toBe(true);
    const runtime = await (await page.request.get('/v1/settings')).json();
    expect(runtime.active.tcp_listen).toBe('127.0.0.1:17444');
    expect(runtime.active.agent_endpoint).toBe('localhost:17444');
    expect(runtime.active.max_concurrent_builds).toBe(3);
    expect(runtime.agent_listener.listening).toBe(true);
    expect(runtime.restart_required).toBe(false);
    const cookie = (await context.cookies()).find((c) => c.name === '__Host-pier_session')!;
    expect(cookie.httpOnly).toBe(true);
    expect(cookie.secure).toBe(true);
    expect(cookie.sameSite).toBe('Strict');
    expect(await page.evaluate(() => document.cookie)).not.toContain('pier_session');
    expect(await page.evaluate(() => Object.keys(localStorage))).toEqual([]);
    expect(
      (
        await page.request.post('/v1/agents', {
          headers: { Origin: 'https://localhost:8444' },
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
    await page.getByRole('button', { name: '保存运行设置', exact: true }).click();
    await expect(page.getByText('已保存，重启后生效', { exact: true })).toBeVisible();
    const saved = await (await page.request.get('/v1/settings')).json();
    expect(saved.active.max_concurrent_builds).toBe(3);
    expect(saved.saved.max_concurrent_builds).toBe(4);
    expect(saved.restart_required).toBe(true);
    await page.reload();
    await expect(page.getByLabel('构建并发数', { exact: true })).toHaveValue('4');
    await page.getByLabel('构建并发数', { exact: true }).fill('3');
    await page.getByRole('button', { name: '保存运行设置', exact: true }).click();
    await expect(page.getByText('已保存，重启后生效', { exact: true })).toHaveCount(0);
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
      public_url: 'https://localhost:8444',
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
    await context.grantPermissions(['clipboard-read', 'clipboard-write']);
    await page.getByRole('button', { name: '复制配对凭据' }).click();
    expect(await page.evaluate(() => navigator.clipboard.readText())).toMatch(/^pier-pair-v2\./);
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
