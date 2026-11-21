#!/usr/bin/env node
// A content mapper for tsrs-lint's integration tests: TypeScript 7.1 content-mapper protocol (JSON-RPC 2.0 over stdio
// with Content-Length framing). Positions are UTF-8 byte offsets. The virtual text of a file is:
//
// - a synthesized prefix with an unresolved import, a floating promise and a helper `__on`,
// - the original text up to a line `//!supplemental`, mapped verbatim, except that each `$gen` becomes the synthesized
//   call `__gen()`, which no mapping covers, as a framework replaces a name with generated code, and each `$return`
//   becomes the synthesized `throw` (`throw undefined` before a `;`), which no mapping covers, as astro2tsx replaces a
//   top-level `return` of the frontmatter,
// - for each line `//!on NAME`, the synthesized statement `__on(() => NAME());`, mapped as one atom to NAME, as a
//   framework maps an event handler wrapper to its attribute,
// - for each line `//!handler EXPRESSION`, the synthesized statement `__on(() => { return (EXPRESSION); });`, with
//   EXPRESSION mapped verbatim and the code before and after it mapped as two atoms to `handler`, as the Vue mapper
//   wraps an inline event handler and maps the wrapper to the tag name,
// - for each line `//!call NAME`, the synthesized statement `NAME();`, with `NAME()` mapped as one atom to NAME,
// - for each line `//!ignored-call NAME`, the same statement and an Ignore directive over it, as a framework marks
//   generated code without a verified mapping (the Vue mapper does this for the slots of a component).
//
// A line `//!ignore-next-line` gets an Ignore directive over the virtual text of the next line, as a user directive
// such as Vue's `@vue-ignore` covers the next element.
//
// The original text after `//!supplemental` is a supplemental output, mapped verbatim after a synthesized `export {};`.
// A file that contains `//!invalid-mapping` gets a mapping that points outside its original text, which the compiler
// rejects as a failed transform.
import process from 'node:process'

const PREFIX =
  'import "./missing-generated-module";\n' +
  'function __on(cb: () => void): void { cb(); }\n' +
  'async function __gen(): Promise<void> {}\n' +
  '__gen();\n'
const SUPPLEMENTAL_MARKER = '//!supplemental\n'

const bytes = (text) => Buffer.byteLength(text, 'utf8')

function respond(message) {
  const body = Buffer.from(JSON.stringify({ jsonrpc: '2.0', ...message }))
  process.stdout.write(`Content-Length: ${body.length}\r\n\r\n`)
  process.stdout.write(body)
}

function transform(params) {
  const content = params.content
  const split = content.indexOf(SUPPLEMENTAL_MARKER)
  const main = split === -1 ? content : content.slice(0, split)
  let text = PREFIX
  // [virtualStart, virtualLength, originalStart, originalLength, kind] with kind 0 = verbatim, 1 = atom
  const mappings = []
  for (let at = 0; ; ) {
    const match = /\$gen|\$return/.exec(main.slice(at))
    const piece = main.slice(at, match ? at + match.index : main.length)
    if (piece.length) mappings.push([bytes(text), bytes(piece), bytes(main.slice(0, at)), bytes(piece), 0])
    text += piece
    if (!match) break
    at += match.index + match[0].length
    text += match[0] === '$gen' ? '__gen()' : main[at] === ';' ? 'throw undefined' : 'throw'
  }
  for (const match of main.matchAll(/^\/\/!on (\w+)$/gm)) {
    const name = match[1]
    const nameStart = bytes(main.slice(0, match.index + '//!on '.length))
    const statement = `__on(() => ${name}());`
    mappings.push([bytes(text), bytes(statement), nameStart, bytes(name), 1])
    text += statement + '\n'
  }
  for (const match of main.matchAll(/^\/\/!handler (.+)$/gm)) {
    const expression = match[1]
    const markerStart = bytes(main.slice(0, match.index + '//!'.length))
    const expressionStart = bytes(main.slice(0, match.index + '//!handler '.length))
    const before = '__on(() => { return ('
    const after = '); });'
    mappings.push([bytes(text), bytes(before), markerStart, bytes('handler'), 1])
    text += before
    mappings.push([bytes(text), bytes(expression), expressionStart, bytes(expression), 0])
    text += expression
    mappings.push([bytes(text), bytes(after), markerStart, bytes('handler'), 1])
    text += after + '\n'
  }
  // [originalStart, originalLength, virtualStart, virtualEnd, policy] with policy 0 = ignore
  const directives = []
  for (const match of main.matchAll(/^\/\/!(call|ignored-call) (\w+)$/gm)) {
    const name = match[2]
    const nameStart = bytes(main.slice(0, match.index + `//!${match[1]} `.length))
    const call = `${name}()`
    const start = bytes(text)
    mappings.push([start, bytes(call), nameStart, bytes(name), 1])
    if (match[1] === 'ignored-call') directives.push([0, 0, start, start + bytes(call) + 1, 0])
    text += call + ';\n'
  }
  const toVirtual = (original) => {
    const mapping = mappings.find(
      ([, length, start, , kind]) => kind === 0 && start <= original && original < start + length,
    )
    return mapping[0] + original - mapping[2]
  }
  for (const match of main.matchAll(/^\/\/!ignore-next-line\n(.*)$/gm)) {
    const lineStart = bytes(main.slice(0, match.index + '//!ignore-next-line\n'.length))
    const virtualStart = toVirtual(lineStart)
    const markerStart = bytes(main.slice(0, match.index))
    directives.push([markerStart, bytes('//!ignore-next-line'), virtualStart, virtualStart + bytes(match[1]), 0])
  }
  if (content.includes('//!invalid-mapping')) {
    mappings.push([0, 1, bytes(content) + 1, 1, 0])
  }
  const result = { text, extension: '.ts', mappings }
  if (directives.length) result.diagnosticDirectives = { unusedExpectDirectiveDiagnostics: [], directives }
  if (split !== -1) {
    const rest = content.slice(split + SUPPLEMENTAL_MARKER.length)
    const header = 'export {};\n'
    const originalStart = bytes(content.slice(0, split + SUPPLEMENTAL_MARKER.length))
    result.supplemental = [
      { text: header + rest, extension: '.ts', mappings: [[bytes(header), bytes(rest), originalStart, bytes(rest), 0]] },
    ]
  }
  return result
}

function handle(request) {
  if (request.id === undefined) return
  switch (request.method) {
    case 'initialize':
      respond({ id: request.id, result: { positionEncoding: 'utf-8', diagnosticSource: 'lint' } })
      break
    case 'openProject': {
      // The option `unknownOption` gets an option diagnostic, as a framework rejects an option that it does not know.
      const optionDiagnostics =
        request.params.options?.unknownOption === undefined
          ? []
          : [{ path: ['unknownOption'], messageText: "Unknown option 'unknownOption'.", code: 1 }]
      respond({ id: request.id, result: { optionDiagnostics } })
      break
    }
    case 'closeProject':
      respond({ id: request.id, result: null })
      break
    case 'transform':
      respond({ id: request.id, result: transform(request.params) })
      break
    default:
      respond({ id: request.id, error: { code: -32601, message: `Unknown method: ${request.method}` } })
  }
}

let pending = Buffer.alloc(0)
process.stdin.on('data', (chunk) => {
  pending = Buffer.concat([pending, chunk])
  for (;;) {
    const headerEnd = pending.indexOf('\r\n\r\n')
    if (headerEnd === -1) return
    const match = /Content-Length:\s*(\d+)/i.exec(pending.subarray(0, headerEnd).toString())
    if (!match) throw new Error('missing Content-Length')
    const length = Number(match[1])
    const start = headerEnd + 4
    if (pending.length < start + length) return
    const body = pending.subarray(start, start + length).toString()
    pending = pending.subarray(start + length)
    handle(JSON.parse(body))
  }
})
process.stdin.on('end', () => process.exit(0))
