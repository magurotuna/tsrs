#!/usr/bin/env node
// A content mapper for tsrs-lint's integration tests: TypeScript 7.1 content-mapper protocol (JSON-RPC 2.0 over stdio
// with Content-Length framing). Positions are UTF-8 byte offsets. The virtual text of a file is:
//
// - a synthesized prefix with an unresolved import, a floating promise and a helper `__on`,
// - the original text up to a line `//!supplemental`, mapped verbatim, except that each `$gen` becomes the synthesized
//   call `__gen()`, which no mapping covers, as a framework replaces a name with generated code,
// - for each line `//!on NAME`, the synthesized statement `__on(() => NAME());`, mapped as one atom to NAME, as a
//   framework maps an event handler wrapper to its attribute.
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
    const gen = main.indexOf('$gen', at)
    const piece = main.slice(at, gen === -1 ? main.length : gen)
    if (piece.length) mappings.push([bytes(text), bytes(piece), bytes(main.slice(0, at)), bytes(piece), 0])
    text += piece
    if (gen === -1) break
    text += '__gen()'
    at = gen + '$gen'.length
  }
  for (const match of main.matchAll(/^\/\/!on (\w+)$/gm)) {
    const name = match[1]
    const nameStart = bytes(main.slice(0, match.index + '//!on '.length))
    const statement = `__on(() => ${name}());`
    mappings.push([bytes(text), bytes(statement), nameStart, bytes(name), 1])
    text += statement + '\n'
  }
  if (content.includes('//!invalid-mapping')) {
    mappings.push([0, 1, bytes(content) + 1, 1, 0])
  }
  const result = { text, extension: '.ts', mappings }
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
