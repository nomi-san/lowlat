// Bundle src/main.ts into dist/main.js. With --serve, watch and serve the
// page from this directory on a loopback address, which browsers treat as a
// secure context: the decoder API the page relies on is unavailable to a page
// served over plain HTTP from anywhere else.
import * as esbuild from 'esbuild'

const options = {
  entryPoints: ['src/main.ts'],
  bundle: true,
  format: 'esm',
  target: 'es2022',
  outdir: 'dist',
  sourcemap: true,
  logLevel: 'info',
}

if (process.argv.includes('--serve')) {
  const ctx = await esbuild.context(options)
  await ctx.watch()
  const { hosts, port } = await ctx.serve({ servedir: '.', host: '127.0.0.1', port: 8000 })
  console.log(`serving http://${hosts[0]}:${port}/`)
} else {
  await esbuild.build(options)
}
