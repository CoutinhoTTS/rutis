# 写一个 TypeScript 插件

从创建项目到发布，再到被宿主使用。JavaScript 写法相同，去掉类型即可。

## 1. 创建项目

```bash
npx @arcships/rutis-host new greeter --lang node
cd greeter
npm install
```

得到的项目：

| 文件 | 作用 |
| --- | --- |
| `src/index.ts` | 插件本身 |
| `test/index.test.ts` | 单元测试，不需要宿主 |
| `rutis.dev.json` | 本地运行时的配置和测试用的其他插件 |
| `package.json` | 依赖 `@arcships/rutis`；开发依赖 `@arcships/rutis-host`、`@arcships/rutis-runtime` |
| `.github/workflows/publish.yml` | 打 `v*` tag 时测试并发布到 npm |

## 2. 写插件

```ts
import { definePlugin } from '@arcships/rutis'

export interface Config {
  city?: string
}

interface Llm {
  ask(question: string): Promise<string>
}

class Weather {
  constructor(private readonly llm: Llm, private readonly city: string) {}

  async today(): Promise<string> {
    return `${await this.llm.ask(`weather in ${this.city}`)} in ${this.city}`
  }
}

export default definePlugin<Config>({
  inject: ['llm'],                                 // 用到的服务：都就绪才启动，任何一个撤销就停下
  provides: { weather: { today: 'async' } },       // 提供的服务，每个方法是 sync 还是 async
  config: { type: 'object', properties: { city: { type: 'string' } } },  // 配置的 JSON Schema
  apply(ctx, config) {
    ctx.provide('weather', new Weather(ctx.use<Llm>('llm'), config.city ?? 'Oslo'))
    return () => {}                                // 清理；也可以用 ctx.effect(cleanup)，或不返回
  },
})
```

插件只依赖 `@arcships/rutis`。它不认识宿主是谁、`llm` 由哪种语言或哪台机器提供，也不管自己什么时候启动和停止：这些由宿主决定。能做什么、值怎样传递，见 [插件 API](plugin-api.md)。

## 3. 测试

`@arcships/rutis/testing` 不需要宿主，给插件提供假服务、调用它提供的服务：

```ts
import { test } from 'node:test'
import assert from 'node:assert/strict'
import { load } from '@arcships/rutis/testing'
import plugin from '../src/index.ts'

test('reports the weather of the configured city', async () => {
  const t = await load(plugin, {
    config: { city: 'Oslo' },
    services: { llm: { ask: async () => 'sunny' } },
  })
  assert.equal(await t.service('weather').today(), 'sunny in Oslo')
  await t.unload()
})
```

```bash
npm test
```

`load` 会检查宿主会检查的事：插件只用 `inject` 里声明的服务；提供的服务带着 `provides` 里声明的每个方法；卸载时清理函数都运行了。默认的严格模式下，值按跨进程的规则传递：数据被复制，函数和带方法的对象按引用传递，声明为 `sync` 的方法不能返回 Promise。只在同一进程里才成立的写法，在这里就会失败。

## 4. 在本地宿主里运行

```bash
npx rutis-host dev
```

`dev` 把这个项目作为一行运行（源码 `src/index.ts`，不需要先构建），改了文件就重新加载它。插件需要的其他服务写在 `rutis.dev.json` 里，例如一个假的 `llm`：

```json
{
  "rows": [
    { "id": "greeter", "config": { "city": "Oslo" } },
    { "id": "llm", "name": "./dev/fake-llm.ts" }
  ]
}
```

```ts
// dev/fake-llm.ts
import { definePlugin } from '@arcships/rutis'
export default definePlugin({
  provides: { llm: { ask: 'async' } },
  apply(ctx) { ctx.provide('llm', { ask: async () => 'sunny' }) },
})
```

和插件 id 相同的那一行给插件本身加配置；其他行是一起运行的插件。`npx rutis-host check` 列出每一行的依赖、提供的服务和配置 Schema，有问题时以非零状态退出，适合放进 CI。

## 5. 发布

```bash
npm run build      # tsc 生成 dist/
npm publish
```

或者推一个 `v0.1.0` 这样的 tag，模板里的工作流会测试、`check` 并发布（仓库里需要 `NPM_TOKEN` secret）。

依赖写 `"@arcships/rutis": "^0.7.0"` 这样的范围。插件上带有它所用的插件 API 版本（`definePlugin` 自动标记），宿主的运行时比它旧时会明确报错，而不是加载后出错。

## 6. 被宿主使用

宿主把插件包装进它的 Node 项目（和 `@arcships/rutis-runtime` 同一处），然后在配置里加一行，行名就是包名：

```bash
npm install greeter
```

```json
{ "id": "greeter", "name": "greeter", "config": { "city": "Oslo" } }
```

见 [rutis-host 与 rutis.json](rutis-host.md)；Rust 宿主见 [在 Rust 应用里嵌入](rust-host.md)。

## 已有的 Cordis 插件

不用改写。在插件包的 `package.json` 里声明要提供给 rutis 的服务即可：

```json
"rutis": { "provides": { "weather": { "today": "async" } } }
```

它在 Node 运行时的 Cordis Context 里运行，和叶子插件在同一个进程，互相用服务时直接拿到对象本身。
