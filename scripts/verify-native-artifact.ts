#!/usr/bin/env node
import { createHash } from 'node:crypto';
import { closeSync, createReadStream, fstatSync, openSync, readFileSync, readSync } from 'node:fs';
import { basename } from 'node:path';

const args: string[] = process.argv.slice(2);
const [target, file, ...extra] = args;
if (!target || !file || extra.length) {
  throw new Error('Usage: verify-native-artifact.ts <target-triple> <binary>');
}

interface NativeArtifactMetadata {
  target: string;
  file: string;
  bytes: number;
  sha256: string;
  /** LC_SYMTAB entry count; Mach-O only. */
  symbols?: number;
}

// A truncated executable with a valid header is the failure mode this gate
// exists to catch, so require a realistic size and validate the format's
// structural tables against the actual file length.
const MINIMUM_BYTES = 1024 * 1024;

// Cheapest strip proxy: the release profile sets `strip = "symbols"`
// (Cargo.toml), which leaves ~330 LC_SYMTAB entries where a debug build
// declares ~293,900. The ceiling is ~30x the measured release figure, so
// toolchain noise cannot reach it, and ~30x below any debug build.
const MAXIMUM_SYMBOLS = 10_000;

// The e2e seams are `cfg(any(debug_assertions, feature = "e2e-seams"))`-gated,
// so no release binary can contain their literals. Longest first, so a shorter
// match can never be mistaken for the whole literal.
const SEAM_LITERALS = ['OO_E2E_PAIR_PIN', 'OO_INSTANCE', 'OO_E2E'];

// The macOS deployment floor is declared once, in .cargo/config.toml's `[env]`.
// A wrong SDK or a drifted deployment target changes minos without changing
// the file format, so it is asserted against that declaration.
let macosFloor = '';
if (target === 'aarch64-apple-darwin') {
  const declared = /^ *MACOSX_DEPLOYMENT_TARGET *= *"(\d+\.\d+)"/mu.exec(
    readFileSync(new URL('../.cargo/config.toml', import.meta.url), 'utf8'),
  )?.[1];
  if (declared === undefined) throw new Error('MACOSX_DEPLOYMENT_TARGET is not declared in .cargo/config.toml');
  macosFloor = declared;
}

/**
 * Walks a Mach-O's load commands: every segment body must lie inside the file,
 * the deployment target must match the floor declared in .cargo/config.toml,
 * and the symbol table must be the stripped one. Returns the LC_SYMTAB entry
 * count, or undefined when the binary carries none.
 */
function checkMachoLoadCommands(descriptor: number, path: string, size: number, ncmds: number, sizeofcmds: number): number | undefined {
  // The load commands occupy the region right after the 32-byte header.
  const commandsEnd = 32 + sizeofcmds;
  if (commandsEnd > size) {
    throw new Error(
      `Mach-O load commands exceed the file length: ${path} (ncmds=${ncmds}, sizeofcmds=${sizeofcmds}, bytes=${size})`,
    );
  }
  const commands = Buffer.alloc(sizeofcmds);
  if (readSync(descriptor, commands, 0, sizeofcmds, 32) !== sizeofcmds) {
    throw new Error(`Truncated Mach-O load commands: ${path}`);
  }
  let symbols: number | undefined;
  let cursor = 0;
  for (let index = 0; index < ncmds; index += 1) {
    if (cursor + 8 > commands.length) throw new Error(`Malformed Mach-O load command table: ${path}`);
    const cmd = commands.readUInt32LE(cursor);
    const cmdSize = commands.readUInt32LE(cursor + 4);
    if (cmdSize < 8 || cursor + cmdSize > commands.length)
      throw new Error(`Malformed Mach-O load command table: ${path}`);
    if (cmd === 0x19) {
      // LC_SEGMENT_64: fileoff at +40, filesize at +48.
      const segmentEnd =
        Number(commands.readBigUInt64LE(cursor + 40)) + Number(commands.readBigUInt64LE(cursor + 48));
      if (segmentEnd > size)
        throw new Error(`Mach-O segment body exceeds the file length: ${path} (segment at +${cursor}, bytes=${size})`);
    }
    // LC_SYMTAB: nsyms at +12. Both commands read here are 24 bytes long.
    if (cmd === 0x2 && cmdSize >= 24) symbols = commands.readUInt32LE(cursor + 12);
    // LC_BUILD_VERSION: platform at +8, minos as packed xxxx.yy.zz at +12.
    if (cmd === 0x32 && cmdSize >= 24) {
      const platform = commands.readUInt32LE(cursor + 8);
      const minos = commands.readUInt32LE(cursor + 12);
      const version = `${minos >>> 16}.${(minos >>> 8) & 0xff}`;
      if (platform !== 1) throw new Error(`Mach-O was not built for macOS (platform ${platform}): ${path}`);
      if (version !== macosFloor)
        throw new Error(
          `Mach-O deployment target ${version} does not match MACOSX_DEPLOYMENT_TARGET ${macosFloor} in .cargo/config.toml: ${path}`,
        );
    }
    cursor += cmdSize;
  }
  if (symbols !== undefined && symbols > MAXIMUM_SYMBOLS) {
    throw new Error(`Mach-O is not stripped: ${path} declares ${symbols} symbols, limit ${MAXIMUM_SYMBOLS}`);
  }
  return symbols;
}

const descriptor = openSync(file, 'r');
let size: number;
// Mach-O symbol-table entries, written by the LC_SYMTAB scan below
let symbols: number | undefined;
try {
  const stat = fstatSync(descriptor);
  if (!stat.isFile() || stat.size < MINIMUM_BYTES) {
    throw new Error(
      `Not a plausible native executable: ${file} is ${stat.size} bytes, minimum ${MINIMUM_BYTES}`,
    );
  }
  size = stat.size;
  const header = Buffer.alloc(64);
  if (readSync(descriptor, header, 0, header.length, 0) !== header.length) {
    throw new Error(`Truncated native header: ${file}`);
  }

  let valid = false;
  switch (target) {
    case 'x86_64-unknown-linux-gnu':
    case 'aarch64-unknown-linux-gnu': {
      const machine = target === 'x86_64-unknown-linux-gnu' ? 62 : 183;
      if (header.subarray(0, 6).equals(Buffer.from('7f454c460201', 'hex'))
        && [2, 3].includes(header.readUInt16LE(16))
        && header.readUInt16LE(18) === machine) {
        // ELF64 program/section header tables must live inside the file.
        const phoff = header.readBigUInt64LE(32);
        const phentsize = header.readUInt16LE(54);
        const phnum = header.readUInt16LE(56);
        const shoff = header.readBigUInt64LE(40);
        const shentsize = header.readUInt16LE(58);
        const shnum = header.readUInt16LE(60);
        const phEnd = phoff + BigInt(phnum) * BigInt(phentsize);
        const shEnd = shoff + BigInt(shnum) * BigInt(shentsize);
        valid = phEnd <= BigInt(size) && shEnd <= BigInt(size);
        if (!valid) {
          throw new Error(
            `ELF header tables exceed the file length: ${file} (phoff=${phoff}, shoff=${shoff}, bytes=${size})`,
          );
        }
      }
      break;
    }
    case 'aarch64-apple-darwin': {
      if (header.readUInt32LE(0) !== 0xfeedfacf
        || header.readUInt32LE(4) !== 0x0100000c
        || header.readUInt32LE(12) !== 2) break;
      symbols = checkMachoLoadCommands(
        descriptor,
        file,
        size,
        header.readUInt32LE(16),
        header.readUInt32LE(20),
      );
      valid = true;
      break;
    }
    case 'x86_64-pc-windows-msvc':
    case 'aarch64-pc-windows-msvc': {
      const machine = target === 'x86_64-pc-windows-msvc' ? 0x8664 : 0xaa64;
      const offset = header.readUInt32LE(60);
      if (header.readUInt16LE(0) !== 0x5a4d || offset < 64 || offset > size - 26) break;
      const pe = Buffer.alloc(26);
      if (readSync(descriptor, pe, 0, pe.length, offset) === pe.length
        && pe.readUInt32LE(0) === 0x00004550
        && pe.readUInt16LE(4) === machine
        && (pe.readUInt16LE(22) & 2) !== 0
        && pe.readUInt16LE(24) === 0x020b) {
        // The PE signature is 4 bytes, so the COFF header starts at +4:
        // number of sections at +6, optional-header size at +20, and the
        // optional header (magic at +24) after it.
        const sections = pe.readUInt16LE(6);
        const optionalSize = pe.readUInt16LE(20);
        const optionalStart = offset + 24;
        const sectionTableOffset = optionalStart + optionalSize;
        const sectionTableEnd = sectionTableOffset + sections * 40;
        valid = sectionTableEnd <= size;
        if (!valid) {
          throw new Error(
            `PE section table exceeds the file length: ${file} (sections=${sections}, offset=${sectionTableOffset}, bytes=${size})`,
          );
        }
      }
      break;
    }
    default:
      throw new Error(`Unsupported release target: ${target}`);
  }
  if (!valid) {
    throw new Error(`Executable format/architecture does not match ${target}: ${file}`);
  }
} finally {
  closeSync(descriptor);
}

const hash = createHash('sha256');
const stream: AsyncIterable<Buffer> = createReadStream(file);
// Overlap the chunks so a literal split across two of them is still found.
const overlap = SEAM_LITERALS[0].length - 1;
let carry = Buffer.alloc(0);
for await (const chunk of stream) {
  hash.update(chunk);
  const window = carry.length === 0 ? chunk : Buffer.concat([carry, chunk]);
  const seam = SEAM_LITERALS.find((literal) => window.includes(literal));
  if (seam !== undefined) {
    throw new Error(`Release artifact contains the e2e seam literal ${seam}: ${file}`);
  }
  carry = Buffer.from(window.subarray(Math.max(0, window.length - overlap)));
}
const metadata: NativeArtifactMetadata = {
  target,
  file: basename(file),
  bytes: size,
  sha256: hash.digest('hex'),
  ...(symbols === undefined ? {} : { symbols }),
};
console.log(JSON.stringify(metadata, null, 2));
