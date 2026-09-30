import { readFileSync, writeFileSync } from 'node:fs';

// Some Ant Design utilities (scrollbar measurement and portal scroll locks)
// do not receive ConfigProvider.csp: https://github.com/react-component/util/issues/613
// Supply the document nonce only at this trusted library's style creation point.
// Keep the patch narrow and fail on dependency changes that need another review.
const packageUrl = new URL('../node_modules/@rc-component/util/package.json', import.meta.url);
const styleUrl = new URL(
  '../node_modules/@rc-component/util/es/Dom/dynamicCSS.js',
  import.meta.url,
);
const { version } = JSON.parse(readFileSync(packageUrl, 'utf8'));
if (version !== '1.13.0')
  throw new Error(`Review the CSP nonce patch for @rc-component/util ${version}`);
const original = "const styleNode = document.createElement('style');";
const patched = `${original}\n  styleNode.nonce = document.querySelector('meta[name="csp-nonce"]')?.content || '';`;
const source = readFileSync(styleUrl, 'utf8');
if (!source.includes(patched)) {
  if (source.split(original).length !== 2)
    throw new Error('Unexpected dynamicCSS source; review the CSP nonce patch');
  writeFileSync(styleUrl, source.replace(original, patched));
}

// xterm's three private style creation sites also need the document nonce.
const xtermRoot = new URL('../node_modules/@xterm/xterm/', import.meta.url);
if (JSON.parse(readFileSync(new URL('package.json', xtermRoot), 'utf8')).version !== '6.0.0')
  throw new Error('Review the CSP nonce patch for xterm');
for (const file of ['lib/xterm.mjs', 'lib/xterm.js']) {
  const count = file.endsWith('.mjs') ? 3 : 4;
  const path = new URL(file, xtermRoot);
  const text = readFileSync(path, 'utf8');
  const marker = '/*pier-xterm-nonce*/';
  if (text.includes(marker)) {
    if (text.split(marker).length !== count + 1) throw new Error('Incomplete xterm nonce patch');
    continue;
  }
  const style = /[a-zA-Z_$][\w$]*(?:\.[a-zA-Z_$][\w$]*)*\.createElement\("style"\)/g;
  if ([...text.matchAll(style)].length !== count)
    throw new Error('Unexpected xterm style creation sites');
  writeFileSync(
    path,
    text.replace(
      style,
      (expression) =>
        `${marker}Object.assign(${expression},{nonce:document.querySelector('meta[name="csp-nonce"]')?.content||''})`,
    ),
  );
}
