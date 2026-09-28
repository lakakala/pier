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
