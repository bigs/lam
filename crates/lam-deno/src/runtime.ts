import {
  op_lam_call,
  op_lam_console,
  op_lam_manifest,
} from "ext:core/ops";

type JsonValue =
  | null
  | boolean
  | number
  | string
  | JsonValue[]
  | { [key: string]: JsonValue };

type BuiltinCallResult =
  | { ok: true; value: JsonValue }
  | { ok: false; error: JsonValue };

interface DirQuery {
  path?: string;
}

interface FunctionDescriptor {
  name: string;
  docs: string;
  inputSchema: JsonValue;
  outputSchema: JsonValue;
  errorSchema: JsonValue;
}

interface NamespaceDescriptor {
  path: string;
  docs: string;
  functions: FunctionDescriptor[];
}

type ConsoleLevel = "debug" | "log" | "info" | "warn" | "error";

interface LamOps {
  op_lam_call(
    namespace: string,
    functionName: string,
    input: JsonValue,
  ): Promise<BuiltinCallResult>;
  op_lam_console(level: ConsoleLevel, message: string): void;
  op_lam_manifest(query: DirQuery | null): NamespaceDescriptor[];
}

type JsonResolution =
  | { kind: "undefined" }
  | { kind: "json"; value: JsonValue }
  | { kind: "not_serializable"; message: string };

type ExceptionResolution =
  | { kind: "runtime" }
  | { kind: "builtin_failure"; error: JsonValue }
  | { kind: "not_serializable"; message: string };

const call = op_lam_call as LamOps["op_lam_call"];
const writeConsole = op_lam_console as LamOps["op_lam_console"];
const manifest = op_lam_manifest as LamOps["op_lam_manifest"];

// Object-valued domain failures remain the exact values Rust serialized.
// The private WeakMap lets the host distinguish an unhandled builtin failure
// from an arbitrary JavaScript throw without adding a visible marker.
const builtinFailures = new WeakMap<object, JsonValue>();

function isObject(value: unknown): value is object {
  return (
    (typeof value === "object" && value !== null) ||
    typeof value === "function"
  );
}

function normalize(value: unknown): JsonResolution {
  if (value === undefined) {
    return { kind: "undefined" };
  }

  try {
    const encoded = JSON.stringify(value);
    if (encoded === undefined) {
      return {
        kind: "not_serializable",
        message: `The result has type ${typeof value}, which JSON cannot represent`,
      };
    }
    return { kind: "json", value: JSON.parse(encoded) as JsonValue };
  } catch (error) {
    return {
      kind: "not_serializable",
      message: error instanceof Error ? error.message : String(error),
    };
  }
}

function markBuiltinFailure(error: JsonValue): unknown {
  if (isObject(error)) {
    builtinFailures.set(error, error);
    return error;
  }

  // WeakMap cannot brand primitives. Preserve them behind a frozen wrapper;
  // object-shaped Rust errors, the normal case, remain unwrapped.
  const wrapper = Object.freeze({
    name: "LamBuiltinFailure",
    error,
  });
  builtinFailures.set(wrapper, error);
  return wrapper;
}

function resolveException(value: unknown): ExceptionResolution {
  if (!isObject(value) || !builtinFailures.has(value)) {
    return { kind: "runtime" };
  }

  const normalized = normalize(builtinFailures.get(value)!);
  if (normalized.kind === "json") {
    return { kind: "builtin_failure", error: normalized.value };
  }
  return normalized;
}

function invoke(
  namespace: string,
  functionName: string,
  input: unknown,
): Promise<JsonValue> {
  return call(namespace, functionName, (input ?? null) as JsonValue).then(
    (result) => {
      if (result.ok) {
        return result.value;
      }
      throw markBuiltinFailure(result.error);
    },
  );
}

function formatConsoleArg(value: unknown): string {
  if (typeof value === "string") {
    return value;
  }

  try {
    const encoded = JSON.stringify(value);
    return encoded === undefined ? String(value) : encoded;
  } catch {
    try {
      return String(value);
    } catch {
      return "<unprintable>";
    }
  }
}

function captureConsole(level: ConsoleLevel, args: unknown[]): void {
  writeConsole(level, args.map(formatConsoleArg).join(" "));
}

const console = Object.freeze({
  debug: (...args: unknown[]) => captureConsole("debug", args),
  log: (...args: unknown[]) => captureConsole("log", args),
  info: (...args: unknown[]) => captureConsole("info", args),
  warn: (...args: unknown[]) => captureConsole("warn", args),
  error: (...args: unknown[]) => captureConsole("error", args),
});

type NamespaceObject = Record<string, unknown>;

const descriptors = manifest(null);
const roots = new Map<string, NamespaceObject>();
const global = globalThis as unknown as NamespaceObject;

function namespaceObject(path: string): NamespaceObject {
  const parts = path.split(".");
  let parent = global;
  let traversed = "";

  for (const part of parts) {
    traversed = traversed === "" ? part : `${traversed}.${part}`;
    let child = roots.get(traversed);
    if (child === undefined) {
      if (
        parent === global &&
        Object.getOwnPropertyDescriptor(globalThis, part) !== undefined
      ) {
        throw new Error(
          `Lam namespace "${traversed}" conflicts with the existing global "${part}"`,
        );
      }
      child = Object.create(null) as NamespaceObject;
      roots.set(traversed, child);
      Object.defineProperty(parent, part, {
        configurable: false,
        enumerable: true,
        value: child,
        writable: false,
      });
    }
    parent = child;
  }

  return parent;
}

for (const namespace of descriptors) {
  const target = namespaceObject(namespace.path);
  for (const fn of namespace.functions) {
    if (namespace.path === "lam" && fn.name === "dir") {
      continue;
    }

    Object.defineProperty(target, fn.name, {
      configurable: false,
      enumerable: true,
      value: (input: unknown) => invoke(namespace.path, fn.name, input),
      writable: false,
    });
  }
}

const lam = namespaceObject("lam");
Object.defineProperty(lam, "dir", {
  configurable: false,
  enumerable: true,
  value: (query?: DirQuery) => manifest(query ?? null),
  writable: false,
});

Object.defineProperty(globalThis, "console", {
  configurable: false,
  enumerable: true,
  value: console,
  writable: false,
});
Object.defineProperty(globalThis, "__lamResolveEvaluation", {
  configurable: false,
  enumerable: false,
  value: normalize,
  writable: false,
});
Object.defineProperty(globalThis, "__lamResolveException", {
  configurable: false,
  enumerable: false,
  value: resolveException,
  writable: false,
});

// The bootstrap capability must not survive into model-authored programs.
if (!Reflect.deleteProperty(globalThis, "Deno") || "Deno" in globalThis) {
  throw new Error("Lam could not remove the deno_core bootstrap capability");
}

for (const namespace of [...roots.keys()].sort(
  (left, right) => right.length - left.length,
)) {
  const target = roots.get(namespace);
  if (target !== undefined) {
    Object.freeze(target);
  }
}
