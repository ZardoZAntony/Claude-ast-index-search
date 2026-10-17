# HOWTO: find who calls a backend endpoint with `http-calls`

[Русская версия](http-calls.ru.md)

`ast-index http-calls` lists the backend calls the frontend makes — JS/TS/Vue modules and the
inline `<script>` blocks of PHP templates — with the method, the address, the keys sent and the
keys read from the answer. Use it to answer "who calls this endpoint", "what does the page send
there", "which calls change data".

```bash
ast-index http-calls --path src/modules/orders         # one directory, text
ast-index http-calls --format json > calls.json        # everything, for a script
```

Run it from the project root: imports are resolved across the whole index, `--path` only filters
what is printed.

## Find the callers of one endpoint

```bash
ast-index http-calls --format json \
  | jq '.calls[] | select(.url != null and (.url | startswith("/api/v1/orders/")))'
```

A call is reported where the request is made **for the caller**: a call through a wrapper or a
client object appears at the line that calls the wrapper, with the address that line passes —
not inside the wrapper, where the address is only a parameter.

## What is recognised

| Kind | Example | `client` / `via` |
|---|---|---|
| `fetch` | `fetch('/api/x', { method: 'POST', body: JSON.stringify(data) })` | `fetch` |
| axios and its instances | `axios.get(url)`, `api.post(url, data)` with `const api = axios.create()` | `axios`, `axios-instance` / `api.post` |
| an instance passed through a function | `const api = withAuth(axios.create())` — interceptors attached | `axios-instance` |
| jQuery, Bitrix | `$.ajax`, `$.post`, `$.get`, `BX.ajax`, `BX.ajax.runComponentAction`, `BX.ajax.runAction` | `jquery`, `bx.ajax`, … |
| a wrapper function | `function post(url, data) { return api.post(url, data) }`, imported too | `wrapper` / `post` |
| a client object of a factory | `createClient()` returning `{ get(url, config) {…}, post(url, data, config) {…}, request }` | `wrapper` / `createClient.get` |
| a method of the same object | `this.request({ url, method: 'DELETE' })`, component options too (`mounted()` calling `methods`) | `wrapper` / `this.request` |

A wrapper or a client method is recognised by **what it forwards**, not by its name: a method
`get(url, config)` that calls `request({ ...config, method: 'get', url })`, which calls
`instance.request(config)`, takes its HTTP method from the `'get'` literal, its address from
argument 0, the query from `params` of argument 1. `post(url, data, config)` sends argument 1 as
the body. `request(config)` reads `method`, `url`, `data`, `params` from the object literal the
caller passes.

### Where the client object comes from

The receiver of `http.get(…)` is followed to the factory call that made it:

```js
const http = createClient()                                      // a constant, module or local
export function createApi(http = createClient()) { … }           // a parameter's default
export function createApi({ client = createClient() } = {}) { … } // a destructured parameter
const client = injected || createClient()                        // `||`, `??`, `?:`, `as T`, `!`
import { http } from '@/shared/http'                             // an import: `export … from` and
                                                                 // `export { x }` of an import followed
var Api = (function () { return { get(url) {…} } })()            // a module object of an IIFE
```

Exports built from imported ones are followed along the chain: a factory, an index
re-exporting it, `export const http = createClient()` in another module, a wrapper around that
client, the caller of the wrapper.

A client given as a parameter **without** a default value says nothing about what it is: no call
is reported, the address stays under "addresses outside recognised calls".

### How the address is folded

| Written | Shown |
|---|---|
| `'/api/items/'` | `/api/items/` |
| `` `/api/items/${id}/` `` | `/api/items/{id}/` — the rest as `{expression}`, matching a route's `{id}` |
| `ITEMS_ENDPOINT` — a module constant, imported too | its value |
| `ENDPOINTS.list`, `Object.freeze({…})` included | the value of that key |
| `ENDPOINTS[operation]` — a key not known | **one call per value** of the object |
| `itemUrl(id)` with `const itemUrl = (id) => \`/api/items/${id}/\`` | the helper's address |
| `this.apiUrl('/orders/')` — a method of the same object | the method's address |
| `endpoint` — a parameter with a non-empty default | the default (an empty default stays `{endpoint}`); a wrapper called without its address argument too |

When nothing folds, `url` is `null` and `url_expr` holds the expression as written.

## Keys sent and read

- `params` — keys of the object literal sent: the body of `post`/`put`/`patch`, `params` of the
  config, `JSON.stringify(obj)`, `FormData.append('key', …)`.
- `reads` — paths the code reads from the answer: `.then(cb)` (inline or a local function),
  `await` into a variable or a destructuring, success callbacks.
- `response_keys` — the same without the client's envelope: axios' `data` is dropped unless the
  wrapper already returns `response.data`.

## Check that a pattern is recognised

1. `ast-index http-calls --path <the api module>` — the call should be listed with its method.
2. If its address is under **"addresses outside recognised calls"** (`literals` in JSON), the
   call around it was not recognised. Usual reasons:
   - the client comes from a parameter without a default, or from a class (`this.http.get` in a
     class constructed with `new`) — not followed;
   - the wrapper builds the address itself from a parameter (`` get(path) { return api.get(`/api/${path}`) } ``)
     — the call is reported inside the wrapper, with the parameter as a hole;
   - the factory returns its object through something other than `return { … }` / `return
     createOther()` (a class instance, `Object.assign`).
3. Strings an address was folded from (constants, map values, helpers) are no longer listed as
   outside calls; what is left there is worth a look.

## Not recognised

- Classes as clients (`new ApiClient()`, `this.http` set in a constructor).
- Objects nobody can name: returned by a CommonJS `module.exports = function`, an AMD `define`
  callback, a class method. Their methods' calls are reported inside them, with the parameter
  as the address.
- Clients passed around after creation (stored in a store, provided/injected) — except as a
  parameter default.
- Dynamic method names (`http[method](url)`), addresses built in loops.
- A wrapper whose address does not come straight from its parameters (prefix + parameter):
  reported inside the wrapper.
- A wrapper making several calls with the address it is given: its callers get the first one.

## JSON

```json
{
  "path": "src/modules/orders/api.js",
  "line": 12,
  "client": "wrapper",
  "via": "createClient.post",
  "method": "POST",
  "url": "/api/orders/",
  "params": ["offerId", "quantity"],
  "reads": ["data.order"],
  "response_keys": ["data.order"]
}
```

`component`, `action`, `mode` appear for Bitrix component actions; `url_expr` when the address
does not fold. `literals` lists addresses outside recognised calls; `globals` the `window.X`
data a template hands to scripts.
