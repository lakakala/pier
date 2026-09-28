import { mkdtempSync, readFileSync, readdirSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, relative } from 'node:path';
import { execFileSync } from 'node:child_process';
const temporary = mkdtempSync(join(tmpdir(), 'pier-web-build-'));
function inventory(root, directory = root, result = new Map()) {
  for (const entry of readdirSync(directory, { withFileTypes: true })) {
    const file = join(directory, entry.name);
    if (entry.isDirectory()) inventory(root, file, result);
    else result.set(relative(root, file), readFileSync(file));
  }
  return result;
}
try {
  execFileSync(
    process.execPath,
    ['node_modules/vite/bin/vite.js', 'build', '--outDir', temporary, '--emptyOutDir'],
    { stdio: 'inherit' },
  );
  const current = inventory('dist');
  const rebuilt = inventory(temporary);
  if (
    current.size !== rebuilt.size ||
    [...current].some(([file, data]) => !rebuilt.get(file)?.equals(data))
  )
    throw new Error('Embedded assets are stale. Run npm run build.');
  console.log('Embedded web assets match the source and lockfile.');
} finally {
  rmSync(temporary, { recursive: true, force: true });
}
