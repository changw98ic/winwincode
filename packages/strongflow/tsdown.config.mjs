import { defineConfig } from 'tsdown'

const packageId = '@winwincode/strongflow'

export default defineConfig({
  name: `${packageId}/client`,
  entry: { client: 'src/client.ts' },
  outDir: 'dist',
  format: 'cjs',
  platform: 'browser',
  target: 'es2024',
  dts: false,
  clean: false,
  sourcemap: true,
  deps: {
    neverBundle: dependency => dependency === 'react' || dependency.startsWith('react/'),
    alwaysBundle: dependency => dependency.startsWith('@winwincode/'),
  },
  outputOptions: {
    entryFileNames: 'client.js',
    banner: `window.__ModuleLoader__.load({ id: ${JSON.stringify(packageId)}, factory: (require) => {`,
    intro: 'var module = { exports: {} }; var exports = module.exports;',
    footer: 'return module.exports; } });',
  },
})
