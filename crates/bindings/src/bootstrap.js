// Realm bootstrap, evaluated once at install time. Returns an object of
// helper functions the Rust side keeps private references to — nothing here
// is reachable from page script except the globals installed explicitly
// (DOMException, queueMicrotask).
(() => {
    "use strict";

    // === Pristine built-in references ===
    // Capture the built-ins the engine's bookkeeping (wrapper cache, collection
    // / style proxies, header normalization) relies on, so page script that
    // later reassigns e.g. `Map.prototype.set`, `WeakRef.prototype.deref`, or
    // `Reflect.get` cannot hijack that bookkeeping. Called as `fn.call(recv, …)`.
    const MapCtor = Map;
    const mapGet = Map.prototype.get;
    const mapSet = Map.prototype.set;
    const mapHas = Map.prototype.has;
    const mapDelete = Map.prototype.delete;
    const WeakRefCtor = WeakRef;
    const weakRefDeref = WeakRef.prototype.deref;
    const WeakMapCtor = WeakMap;
    const weakMapGet = WeakMap.prototype.get;
    const weakMapSet = WeakMap.prototype.set;
    const promiseThen = Promise.prototype.then;
    const reflectGet = Reflect.get;
    const reflectSet = Reflect.set;
    const reflectHas = Reflect.has;
    const reflectOwnKeys = Reflect.ownKeys;
    const reflectGetOwnPropertyDescriptor = Reflect.getOwnPropertyDescriptor;
    const reflectDefineProperty = Reflect.defineProperty;
    const reflectDeleteProperty = Reflect.deleteProperty;
    const arrayIsArray = Array.isArray;
    const arrayFrom = Array.from;
    const arrayMap = Array.prototype.map;
    const objectEntries = Object.entries;
    const objectFreeze = Object.freeze;

    // Extra pristine references for `structuredClone` (page script may later
    // reassign any of these globals or their prototype methods).
    const ObjectCtor = Object;
    const ObjectProto = Object.prototype;
    const getPrototypeOf = Object.getPrototypeOf;
    const objectCreate = Object.create;
    const objectKeys = Object.keys;
    const defineProp = Object.defineProperty;
    const getOwnPropDesc = Object.getOwnPropertyDescriptor;
    const SetCtor = Set;
    const setAdd = Set.prototype.add;
    const setForEach = Set.prototype.forEach;
    const mapForEach = Map.prototype.forEach;
    const DateCtor = Date;
    const dateGetTime = Date.prototype.getTime;
    const RegExpCtor = RegExp;
    const reSourceGet = getOwnPropDesc(RegExp.prototype, "source").get;
    const reFlagsGet = getOwnPropDesc(RegExp.prototype, "flags").get;
    const ArrayBufferCtor = ArrayBuffer;
    const abSlice = ArrayBuffer.prototype.slice;
    const abIsView = ArrayBuffer.isView;
    const DataViewCtor = DataView;
    const dvBufferGet = getOwnPropDesc(DataView.prototype, "buffer").get;
    const dvByteOffsetGet = getOwnPropDesc(DataView.prototype, "byteOffset").get;
    const dvByteLengthGet = getOwnPropDesc(DataView.prototype, "byteLength").get;
    const typedArrayProto = getPrototypeOf(Uint8Array.prototype);
    const taBufferGet = getOwnPropDesc(typedArrayProto, "buffer").get;
    const taByteOffsetGet = getOwnPropDesc(typedArrayProto, "byteOffset").get;
    const taLengthGet = getOwnPropDesc(typedArrayProto, "length").get;
    const typedArrayCtorByProto = new Map();
    for (const Ctor of [
        Int8Array, Uint8Array, Uint8ClampedArray, Int16Array, Uint16Array,
        Int32Array, Uint32Array, Float32Array, Float64Array,
        typeof BigInt64Array === "function" ? BigInt64Array : undefined,
        typeof BigUint64Array === "function" ? BigUint64Array : undefined,
    ]) {
        if (typeof Ctor === "function") mapSet.call(typedArrayCtorByProto, Ctor.prototype, Ctor);
    }
    const BooleanCtor = Boolean;
    const NumberCtor = Number;
    const StringCtor = String;
    const boolValueOf = Boolean.prototype.valueOf;
    const numValueOf = Number.prototype.valueOf;
    const strValueOf = String.prototype.valueOf;
    const ErrorCtor = Error;
    const errorCtorByName = {
        Error, EvalError, RangeError, ReferenceError, SyntaxError, TypeError, URIError,
    };

    // === DOMException (WebIDL: inherits Error) ===
    const legacyCodes = {
        IndexSizeError: 1, HierarchyRequestError: 3, WrongDocumentError: 4,
        InvalidCharacterError: 5, NoModificationAllowedError: 7,
        NotFoundError: 8, NotSupportedError: 9, InUseAttributeError: 10,
        InvalidStateError: 11, SyntaxError: 12, InvalidModificationError: 13,
        NamespaceError: 14, InvalidAccessError: 15, SecurityError: 18,
        NetworkError: 19, AbortError: 20, URLMismatchError: 21,
        QuotaExceededError: 22, TimeoutError: 23, InvalidNodeTypeError: 24,
        DataCloneError: 25,
    };
    class DOMException extends Error {
        #name;
        constructor(message = "", name = "Error") {
            super(String(message));
            this.#name = String(name);
        }
        get name() { return this.#name; }
        get code() { return legacyCodes[this.#name] ?? 0; }
    }
    const constants = {
        INDEX_SIZE_ERR: 1, DOMSTRING_SIZE_ERR: 2, HIERARCHY_REQUEST_ERR: 3,
        WRONG_DOCUMENT_ERR: 4, INVALID_CHARACTER_ERR: 5, NO_DATA_ALLOWED_ERR: 6,
        NO_MODIFICATION_ALLOWED_ERR: 7, NOT_FOUND_ERR: 8, NOT_SUPPORTED_ERR: 9,
        INUSE_ATTRIBUTE_ERR: 10, INVALID_STATE_ERR: 11, SYNTAX_ERR: 12,
        INVALID_MODIFICATION_ERR: 13, NAMESPACE_ERR: 14, INVALID_ACCESS_ERR: 15,
        VALIDATION_ERR: 16, TYPE_MISMATCH_ERR: 17, SECURITY_ERR: 18,
        NETWORK_ERR: 19, ABORT_ERR: 20, URL_MISMATCH_ERR: 21,
        QUOTA_EXCEEDED_ERR: 22, TIMEOUT_ERR: 23, INVALID_NODE_TYPE_ERR: 24,
        DATA_CLONE_ERR: 25,
    };
    for (const [key, value] of Object.entries(constants)) {
        const desc = { value, writable: false, enumerable: true, configurable: false };
        Object.defineProperty(DOMException, key, desc);
        Object.defineProperty(DOMException.prototype, key, desc);
    }
    globalThis.DOMException = DOMException;

    // === queueMicrotask (rides the engine's promise-job queue so ordering
    // with promise reactions is exact) ===
    const resolved = Promise.resolve();
    globalThis.queueMicrotask = function queueMicrotask(callback) {
        if (typeof callback !== "function") {
            throw new TypeError("queueMicrotask: callback is not a function");
        }
        resolved.then(callback);
    };

    // The engine's own microtask enqueuer, over the *pristine* resolved promise
    // so page script cannot intercept it by reassigning `globalThis.
    // queueMicrotask` or patching `Promise.prototype.then`. Used to queue the
    // mutation-observer compound microtask at the exact point the DOM spec
    // says: when the first record is queued, so it is ordered against promise
    // reactions rather than draining after all of them.
    const enqueueMicrotask = (callback) => { promiseThen.call(resolved, callback); };

    // === Helpers returned to the Rust side ===

    // Weak wrapper cache: node-slot index → WeakRef(wrapper).
    const newWrapperMap = () => new MapCtor();
    const cacheGet = (map, key) => {
        const ref = mapGet.call(map, key);
        if (ref === undefined) return undefined;
        const value = weakRefDeref.call(ref);
        if (value === undefined) mapDelete.call(map, key);
        return value;
    };
    const cacheSet = (map, key, value) => { mapSet.call(map, key, new WeakRefCtor(value)); };

    // Indexed collections: a Proxy over the host object adds the WebIDL
    // indexed-property behavior; methods and getters resolve through the
    // target's prototype chain (the Rust side unwraps proxies for brand
    // checks, so `this` may be either the proxy or the target).
    const indexOf = (prop) => {
        if (typeof prop !== "string") return null;
        const n = +prop;
        return Number.isInteger(n) && n >= 0 && String(n) === prop ? n : null;
    };
    const collectionProxy = (target, named) => new Proxy(target, {
        get(t, prop, receiver) {
            const i = indexOf(prop);
            if (i !== null) {
                const v = t.item(i);
                return v === null ? undefined : v;
            }
            if (named && typeof prop === "string" && !reflectHas(t, prop)) {
                const getter = typeof t.namedItem === "function" ? t.namedItem : t.getNamedItem;
                const v = getter.call(t, prop);
                if (v !== null) return v;
            }
            // Read own/inherited members with the target as the receiver, not the
            // proxy: a native accessor invoked via `Reflect.get(target, prop,
            // proxy)` brand-checks its `this`, and a host object's accessor does
            // not accept the wrapping proxy there (Angular Material reads
            // `querySelectorAll(...).length` this way when caching SVG external
            // references). Collections are leaf host objects, so the receiver
            // distinction is not otherwise observable.
            return reflectGet(t, prop, t);
        },
        has(t, prop) {
            const i = indexOf(prop);
            if (i !== null) return t.item(i) !== null;
            if (named && typeof prop === "string" && !reflectHas(t, prop)) {
                return t.namedItem(prop) !== null;
            }
            return reflectHas(t, prop);
        },
        ownKeys(t) {
            const keys = [];
            const len = t.length;
            for (let i = 0; i < len; i++) keys.push(String(i));
            for (const key of reflectOwnKeys(t)) keys.push(key);
            return keys;
        },
        getOwnPropertyDescriptor(t, prop) {
            const i = indexOf(prop);
            if (i !== null) {
                const v = t.item(i);
                if (v === null) return undefined;
                return { value: v, writable: false, enumerable: true, configurable: true };
            }
            return reflectGetOwnPropertyDescriptor(t, prop);
        },
        set(t, prop, value, receiver) {
            if (indexOf(prop) !== null) return false;
            // Target (not the wrapping proxy) as the receiver — see the `get`
            // trap: a host object's native setter rejects the proxy on that path.
            return reflectSet(t, prop, value, t);
        },
        defineProperty(t, prop, desc) {
            if (indexOf(prop) !== null) return false;
            return reflectDefineProperty(t, prop, desc);
        },
    });

    // CSSStyleDeclaration: a Proxy adding CSS-property access by IDL attribute
    // (style.backgroundColor), by dashed name (style["background-color"]), and
    // by index (style[0] === the declared property name). `initStyleProps`
    // seeds the shared key → dashed-property map once at install; keys not in
    // the map fall through to the prototype (methods, cssText, length).
    let styleProps = null;
    const initStyleProps = (pairs) => { styleProps = new MapCtor(pairs); };
    const isCssKey = (t, prop) =>
        typeof prop === "string" && mapHas.call(styleProps, prop) && !reflectHas(t, prop);
    const styleProxy = (target) => new Proxy(target, {
        get(t, prop, receiver) {
            const i = indexOf(prop);
            if (i !== null) return i < t.length ? t.item(i) : undefined;
            if (isCssKey(t, prop)) return t.getPropertyValue(mapGet.call(styleProps, prop));
            // Target as the receiver (not the proxy): a native accessor (e.g.
            // `cssText`, `length`) brand-checks `this` and rejects the wrapping
            // proxy. `CSSStyleDeclaration` is a leaf host object.
            return reflectGet(t, prop, t);
        },
        has(t, prop) {
            const i = indexOf(prop);
            if (i !== null) return i < t.length;
            if (isCssKey(t, prop)) return true;
            return reflectHas(t, prop);
        },
        set(t, prop, value, receiver) {
            if (indexOf(prop) !== null) return false;
            if (isCssKey(t, prop)) {
                t.setProperty(mapGet.call(styleProps, prop), value === null ? "" : String(value));
                return true;
            }
            return reflectSet(t, prop, value, t);
        },
        ownKeys(t) {
            const keys = [];
            const len = t.length;
            for (let i = 0; i < len; i++) keys.push(String(i));
            for (const key of reflectOwnKeys(t)) keys.push(key);
            return keys;
        },
        getOwnPropertyDescriptor(t, prop) {
            const i = indexOf(prop);
            if (i !== null) {
                if (i >= t.length) return undefined;
                return { value: t.item(i), writable: false, enumerable: true, configurable: true };
            }
            return reflectGetOwnPropertyDescriptor(t, prop);
        },
    });

    // DOMStringMap (`element.dataset`): a Proxy exposing the element's `data-*`
    // content attributes as camelCased properties. It is backed by the element
    // wrapper's own attribute methods, so every read/write flows through the one
    // attribute code path (invalidation, custom-element reactions) for free.
    //
    // Name mapping (HTML "domstringmap"): a property name maps to `data-` plus
    // the name with each ASCII uppercase letter replaced by `-` + its lowercase;
    // a `-` immediately before an ASCII lowercase letter is invalid (SyntaxError
    // on write). The reverse drops `data-` and uppercases each letter after a
    // `-`; an attribute whose name has any uppercase does not qualify.
    const propToDataAttr = (prop) => {
        let out = "data-";
        for (let i = 0; i < prop.length; i++) {
            const c = prop.charCodeAt(i);
            if (c === 0x2d && i + 1 < prop.length) {
                const n = prop.charCodeAt(i + 1);
                if (n >= 0x61 && n <= 0x7a) return null; // "-" + a–z
            }
            out += c >= 0x41 && c <= 0x5a ? "-" + prop[i].toLowerCase() : prop[i];
        }
        return out;
    };
    const dataAttrToProp = (attr) => {
        // `attr` is a lowercased content-attribute name starting with `data-`.
        let out = "";
        for (let i = 5; i < attr.length; i++) {
            const c = attr.charCodeAt(i);
            if (c >= 0x41 && c <= 0x5a) return null; // uppercase disqualifies
            if (c === 0x2d && i + 1 < attr.length) {
                const n = attr.charCodeAt(i + 1);
                if (n >= 0x61 && n <= 0x7a) {
                    out += attr[i + 1].toUpperCase();
                    i++;
                    continue;
                }
            }
            out += attr[i];
        }
        return out;
    };
    const syntaxError = () =>
        new DOMException(
            "dataset name must not contain '-' before an ASCII lowercase letter",
            "SyntaxError",
        );
    const datasetProxy = (el, proto) => {
        const target = Object.create(proto);
        // A "data property" is any string key not shadowed by the prototype
        // chain (constructor, toString, Symbol.* stay on the prototype).
        const isData = (prop) => typeof prop === "string" && !reflectHas(target, prop);
        return new Proxy(target, {
            get(t, prop) {
                if (isData(prop)) {
                    const attr = propToDataAttr(prop);
                    const v = attr === null ? null : el.getAttribute(attr);
                    return v === null ? undefined : v;
                }
                return reflectGet(t, prop, t);
            },
            has(t, prop) {
                if (isData(prop)) {
                    const attr = propToDataAttr(prop);
                    return attr !== null && el.hasAttribute(attr);
                }
                return reflectHas(t, prop);
            },
            set(t, prop, value) {
                if (isData(prop)) {
                    const attr = propToDataAttr(prop);
                    if (attr === null) throw syntaxError();
                    el.setAttribute(attr, String(value));
                    return true;
                }
                return reflectSet(t, prop, value, t);
            },
            deleteProperty(t, prop) {
                if (isData(prop)) {
                    const attr = propToDataAttr(prop);
                    if (attr !== null) el.removeAttribute(attr);
                    return true;
                }
                return reflectDeleteProperty(t, prop);
            },
            ownKeys() {
                const keys = [];
                for (const attr of el.getAttributeNames()) {
                    if (attr.length > 5 && attr.startsWith("data-")) {
                        const prop = dataAttrToProp(attr);
                        if (prop !== null) keys.push(prop);
                    }
                }
                return keys;
            },
            getOwnPropertyDescriptor(t, prop) {
                if (isData(prop)) {
                    const attr = propToDataAttr(prop);
                    const v = attr === null ? null : el.getAttribute(attr);
                    if (v === null) return undefined;
                    return { value: v, writable: true, enumerable: true, configurable: true };
                }
                return reflectGetOwnPropertyDescriptor(t, prop);
            },
            defineProperty(t, prop, desc) {
                if (isData(prop)) {
                    if ("get" in desc || "set" in desc) return false;
                    if ("value" in desc) {
                        const attr = propToDataAttr(prop);
                        if (attr === null) throw syntaxError();
                        el.setAttribute(attr, String(desc.value));
                    }
                    return true;
                }
                return reflectDefineProperty(t, prop, desc);
            },
        });
    };

    // WebIDL `iterable<>` members, built on length + indexed access.
    const installIterable = (proto) => {
        const arrayProto = Array.prototype;
        const desc = (value) => ({ value, writable: true, enumerable: true, configurable: true });
        Object.defineProperty(proto, Symbol.iterator, {
            value: arrayProto.values, writable: true, enumerable: false, configurable: true,
        });
        Object.defineProperty(proto, "values", desc(arrayProto.values));
        Object.defineProperty(proto, "keys", desc(arrayProto.keys));
        Object.defineProperty(proto, "entries", desc(arrayProto.entries));
        Object.defineProperty(proto, "forEach", desc(arrayProto.forEach));
    };

    // Interfaces with an indexed property getter but no `iterable<>`
    // declaration (NamedNodeMap, HTMLCollection) still get
    // @@iterator = %Array.prototype.values% per WebIDL — and nothing else
    // (no keys/values/entries/forEach).
    const installValueIterator = (proto) => {
        Object.defineProperty(proto, Symbol.iterator, {
            value: Array.prototype.values, writable: true, enumerable: false, configurable: true,
        });
    };

    // `adoptedStyleSheets` is an ObservableArray: in-place mutations
    // (push, indexed writes, length truncation, delete) must reach the style
    // engine, not only full reassignment. Writes forward to the plain target
    // array, then the native sync re-reads it for the owner scope.
    const adoptedSheetsProxy = (owner, sync, initial) => {
        const target = Array.isArray(initial) ? Array.prototype.slice.call(initial) : [];
        return new Proxy(target, {
            set(t, key, value, receiver) {
                const ok = Reflect.set(t, key, value, receiver);
                if (ok) sync(owner, t);
                return ok;
            },
            deleteProperty(t, key) {
                const ok = Reflect.deleteProperty(t, key);
                if (ok) sync(owner, t);
                return ok;
            },
        });
    };

    const setToStringTag = (proto, name) => {
        Object.defineProperty(proto, Symbol.toStringTag, {
            value: name, writable: false, enumerable: false, configurable: true,
        });
    };

    const makeDomException = (name, message) => new DOMException(message, name);

    // === structuredClone (HTML "structured clone") ===
    // A recursive clone with a memo Map that both breaks cycles and preserves
    // shared identity (two views over one ArrayBuffer stay shared; the same
    // nested object referenced twice clones once). Objects whose prototype is
    // neither a recognized built-in nor `%Object.prototype%`/null — host/slab
    // objects (DOM nodes, etc.) and functions/symbols — raise DataCloneError.
    const cloneError = () =>
        new DOMException("The object could not be cloned.", "DataCloneError");
    const structuredCloneInner = (value, memo) => {
        if (value === null) return null;
        const t = typeof value;
        if (t === "undefined" || t === "boolean" || t === "number"
            || t === "string" || t === "bigint") {
            return value;
        }
        if (t === "symbol" || t === "function") throw cloneError();

        const seen = mapGet.call(memo, value);
        if (seen !== undefined) return seen;
        const proto = getPrototypeOf(value);

        if (arrayIsArray(value)) {
            const out = [];
            mapSet.call(memo, value, out);
            const len = value.length;
            for (let i = 0; i < len; i++) {
                if (i in value) out[i] = structuredCloneInner(value[i], memo);
            }
            // Own enumerable non-index string keys are cloned too (`a.foo`).
            for (const key of objectKeys(value)) {
                if (!(key in out)) out[key] = structuredCloneInner(value[key], memo);
            }
            return out;
        }
        // Built-ins are matched by `instanceof` (captured pristine constructors)
        // so subclass instances clone via their base class, as the spec's
        // internal-slot cloning does — matching the Array/Error handling here.
        if (value instanceof MapCtor) {
            const out = new MapCtor();
            mapSet.call(memo, value, out);
            mapForEach.call(value, (v, k) => {
                mapSet.call(out, structuredCloneInner(k, memo), structuredCloneInner(v, memo));
            });
            return out;
        }
        if (value instanceof SetCtor) {
            const out = new SetCtor();
            mapSet.call(memo, value, out);
            setForEach.call(value, (v) => { setAdd.call(out, structuredCloneInner(v, memo)); });
            return out;
        }
        if (value instanceof DateCtor) {
            const out = new DateCtor(dateGetTime.call(value));
            mapSet.call(memo, value, out);
            return out;
        }
        if (value instanceof RegExpCtor) {
            const out = new RegExpCtor(reSourceGet.call(value), reFlagsGet.call(value));
            mapSet.call(memo, value, out);
            return out;
        }
        if (value instanceof ArrayBufferCtor) {
            const out = abSlice.call(value, 0);
            mapSet.call(memo, value, out);
            return out;
        }
        if (value instanceof DataViewCtor) {
            const clonedBuf = structuredCloneInner(dvBufferGet.call(value), memo);
            const out = new DataViewCtor(
                clonedBuf, dvByteOffsetGet.call(value), dvByteLengthGet.call(value));
            mapSet.call(memo, value, out);
            return out;
        }
        if (abIsView(value)) {
            // A typed array (DataView already handled). Resolve its base
            // constructor by walking the prototype chain, so subclasses clone as
            // the base type.
            let p = proto;
            let TaCtor;
            while (p !== null && TaCtor === undefined) {
                TaCtor = mapGet.call(typedArrayCtorByProto, p);
                p = getPrototypeOf(p);
            }
            if (TaCtor !== undefined) {
                const clonedBuf = structuredCloneInner(taBufferGet.call(value), memo);
                const out = new TaCtor(
                    clonedBuf, taByteOffsetGet.call(value), taLengthGet.call(value));
                mapSet.call(memo, value, out);
                return out;
            }
        }
        if (value instanceof BooleanCtor) {
            const out = ObjectCtor(boolValueOf.call(value));
            mapSet.call(memo, value, out);
            return out;
        }
        if (value instanceof NumberCtor) {
            const out = ObjectCtor(numValueOf.call(value));
            mapSet.call(memo, value, out);
            return out;
        }
        if (value instanceof StringCtor) {
            const out = ObjectCtor(strValueOf.call(value));
            mapSet.call(memo, value, out);
            return out;
        }
        if (value instanceof DOMException) {
            const out = new DOMException(value.message, value.name);
            mapSet.call(memo, value, out);
            return out;
        }
        if (value instanceof ErrorCtor) {
            const name = value.name;
            const Ctor = errorCtorByName[name] || ErrorCtor;
            const out = new Ctor(String(value.message));
            mapSet.call(memo, value, out);
            if (value.stack !== undefined) {
                try { out.stack = value.stack; } catch (e) { /* read-only stack */ }
            }
            if ("cause" in value) out.cause = structuredCloneInner(value.cause, memo);
            if (!(name in errorCtorByName)) {
                defineProp(out, "name", {
                    value: name, writable: true, enumerable: false, configurable: true,
                });
            }
            return out;
        }
        // Plain object (own enumerable string-keyed properties only). Any other
        // exotic prototype is an object we cannot structurally clone.
        if (proto === ObjectProto || proto === null) {
            const out = proto === null ? objectCreate(null) : {};
            mapSet.call(memo, value, out);
            for (const key of objectKeys(value)) {
                out[key] = structuredCloneInner(value[key], memo);
            }
            return out;
        }
        throw cloneError();
    };
    const structuredClone = function structuredClone(value, options) {
        if (options !== null && options !== undefined) {
            const transfer = options.transfer;
            if (transfer !== null && transfer !== undefined) {
                // Transfer would detach the originals — unsupported, so a
                // non-empty list is a DataCloneError rather than a silent no-op.
                const len = transfer.length;
                if (typeof len === "number" && len > 0) throw cloneError();
            }
        }
        return structuredCloneInner(value, new MapCtor());
    };
    globalThis.structuredClone = structuredClone;

    // Deferred-promise construction: fetch/XHR completions (delivered from
    // Rust) call the captured resolve/reject.
    const makePromise = () => {
        let resolve, reject;
        const promise = new Promise((res, rej) => { resolve = res; reject = rej; });
        return { promise, resolve, reject };
    };
    const resolvedPromise = (value) => Promise.resolve(value);

    // Normalizes a headers/params init (record, array of pairs, or iterable)
    // into an array of [name, value] string pairs.
    const recordPairs = (o) => {
        if (o === null || o === undefined) return [];
        if (arrayIsArray(o)) return arrayMap.call(o, (p) => [String(p[0]), String(p[1])]);
        if (typeof o[Symbol.iterator] === "function") {
            return arrayFrom(o, (p) => [String(p[0]), String(p[1])]);
        }
        return arrayMap.call(objectEntries(o), ([k, v]) => [k, String(v)]);
    };

    const freeze = (value) => objectFreeze(value);

    // Runs a custom-element constructor as an upgrade: `Reflect.construct(C,
    // [], C)` sets `new.target = C`, so the QuickJS subclass trampoline pins
    // the resulting object's prototype to `C.prototype`. The native
    // `HTMLElement` base constructor binds it to the pre-created node via the
    // construction stack in `PageState`.
    const ceConstruct = (ctor) => Reflect.construct(ctor, [], ctor);

    // Property removal, which the `JsScope` trait does not expose. Used to
    // retire stale accessors from the Window named properties object.
    const deleteProperty = (o, k) => reflectDeleteProperty(o, k);

    // Globals that depend on generated interface classes / other globals, so
    // they cannot be installed while the bootstrap script first runs. Rust
    // calls this once, after `register_interfaces` and `install_window`.
    const installLateGlobals = () => {
        // `AbortSignal.abort()` / `AbortSignal.timeout()` are static factories
        // (the codegen emits no static operations). Both build a controller and
        // return its signal; the default abort reason (AbortError) is supplied
        // by the native abort algorithm.
        const AbortSignal = globalThis.AbortSignal;
        const AbortController = globalThis.AbortController;
        if (typeof AbortController === "function" && typeof AbortSignal === "function") {
            Object.defineProperty(AbortSignal, "abort", {
                value: function abort(reason) {
                    const controller = new AbortController();
                    controller.abort(reason);
                    return controller.signal;
                },
                writable: true, enumerable: false, configurable: true,
            });
            Object.defineProperty(AbortSignal, "timeout", {
                value: function timeout(milliseconds) {
                    const controller = new AbortController();
                    globalThis.setTimeout(() => {
                        controller.abort(new DOMException("signal timed out", "TimeoutError"));
                    }, milliseconds);
                    return controller.signal;
                },
                writable: true, enumerable: false, configurable: true,
            });
        }

        // performance.mark / measure / getEntries*: a pure-JS user-timing layer
        // over `performance.now()`. navigation/resource/paint entry types are
        // not tracked (they read back as empty).
        const performance = globalThis.performance;
        const Performance = globalThis.Performance;
        if (performance && typeof Performance === "function") {
            const proto = Performance.prototype;
            let entries = [];            // mark + measure entries, in order
            let marksByName = new MapCtor();
            // The realm survives navigation, so the timeline must be reset per
            // document. `navigationStart` is re-stamped on each navigation, so a
            // change signals a new document — clear the buffers before any read.
            let lastNav = performance.timing.navigationStart;
            const maybeReset = () => {
                const nav = performance.timing.navigationStart;
                if (nav !== lastNav) {
                    entries = [];
                    marksByName = new MapCtor();
                    lastNav = nav;
                }
            };
            const makeEntry = (name, entryType, startTime, duration) => objectFreeze({
                name: String(name), entryType, startTime, duration,
                toJSON() {
                    return { name: this.name, entryType, startTime, duration };
                },
            });
            // PerformanceTiming attribute names measure() may reference as marks.
            const timingAttrs = {
                navigationStart: 1, unloadEventStart: 1, unloadEventEnd: 1,
                redirectStart: 1, redirectEnd: 1, fetchStart: 1,
                domainLookupStart: 1, domainLookupEnd: 1, connectStart: 1,
                connectEnd: 1, secureConnectionStart: 1, requestStart: 1,
                responseStart: 1, responseEnd: 1, domLoading: 1, domInteractive: 1,
                domContentLoadedEventStart: 1, domContentLoadedEventEnd: 1,
                domComplete: 1, loadEventStart: 1, loadEventEnd: 1,
            };
            // Resolves a string mark reference to a timestamp: a recorded mark,
            // a non-zero PerformanceTiming attribute, else a SyntaxError.
            const resolveMark = (ref) => {
                const name = String(ref);
                const m = mapGet.call(marksByName, name);
                if (m !== undefined) return m.startTime;
                if (timingAttrs[name] === 1) {
                    const v = performance.timing[name];
                    if (v === 0) {
                        throw new DOMException(
                            "Failed to execute 'measure' on 'Performance': The mark '"
                            + name + "' does not have a non-zero value.", "SyntaxError");
                    }
                    return v - performance.timeOrigin;
                }
                throw new DOMException(
                    "Failed to execute 'measure' on 'Performance': The mark '"
                    + name + "' does not exist.", "SyntaxError");
            };
            const defMethod = (name, value) => {
                Object.defineProperty(proto, name, {
                    value, writable: true, enumerable: true, configurable: true,
                });
            };
            defMethod("mark", function mark(name, options) {
                maybeReset();
                const startTime = (options && typeof options.startTime === "number")
                    ? options.startTime : performance.now();
                const entry = makeEntry(name, "mark", startTime, 0);
                entries.push(entry);
                mapSet.call(marksByName, String(name), entry);
                return entry;
            });
            defMethod("measure", function measure(name, startOrOptions, endMark) {
                maybeReset();
                let startTime = 0;
                let endTime = performance.now();
                if (typeof startOrOptions === "string") {
                    startTime = resolveMark(startOrOptions);
                    if (endMark !== undefined) endTime = resolveMark(endMark);
                } else if (startOrOptions && typeof startOrOptions === "object") {
                    const o = startOrOptions;
                    if (typeof o.start === "number") startTime = o.start;
                    else if (typeof o.start === "string") startTime = resolveMark(o.start);
                    if (typeof o.end === "number") endTime = o.end;
                    else if (typeof o.end === "string") endTime = resolveMark(o.end);
                    if (typeof o.duration === "number") {
                        if (o.start !== undefined) endTime = startTime + o.duration;
                        else startTime = endTime - o.duration;
                    }
                }
                const entry = makeEntry(name, "measure", startTime, endTime - startTime);
                entries.push(entry);
                return entry;
            });
            defMethod("getEntries", function getEntries() {
                maybeReset();
                return entries.slice();
            });
            defMethod("getEntriesByType", function getEntriesByType(type) {
                maybeReset();
                const t = String(type);
                return entries.filter((e) => e.entryType === t);
            });
            defMethod("getEntriesByName", function getEntriesByName(name, type) {
                maybeReset();
                const n = String(name);
                return entries.filter((e) =>
                    e.name === n && (type === undefined || e.entryType === String(type)));
            });
            const clearBy = (entryType, name) => {
                maybeReset();
                for (let i = entries.length - 1; i >= 0; i--) {
                    const e = entries[i];
                    if (e.entryType === entryType && (name === undefined || e.name === String(name))) {
                        entries.splice(i, 1);
                        if (entryType === "mark") mapDelete.call(marksByName, e.name);
                    }
                }
            };
            defMethod("clearMarks", function clearMarks(name) { clearBy("mark", name); });
            defMethod("clearMeasures", function clearMeasures(name) { clearBy("measure", name); });
        }

        // Native helpers installed by `install_native_helpers`, captured here and
        // deleted from the global so page script never sees the `__oxide_*`
        // surface. `randomBytes` backs `crypto`.
        const nativeRandomBytes = globalThis.__oxide_randomBytes;

        const def = (target, name, value) => {
            reflectDefineProperty(target, name, {
                value, writable: true, enumerable: true, configurable: true,
            });
        };
        const defGlobal = (name, value) => {
            if (globalThis[name] === undefined) {
                reflectDefineProperty(globalThis, name, {
                    value, writable: true, enumerable: true, configurable: true,
                });
            }
        };

        // === crypto (getRandomValues + randomUUID) ===
        // A non-cryptographic-strength note does not apply: `__oxide_randomBytes`
        // draws from the OS CSPRNG. `crypto.subtle` is not implemented (v1).
        if (typeof nativeRandomBytes === "function" && globalThis.crypto === undefined) {
            const getRandomValues = function getRandomValues(view) {
                if (!abIsView(view) || view instanceof DataViewCtor
                    || view instanceof Float32Array || view instanceof Float64Array) {
                    throw new DOMException(
                        "Failed to execute 'getRandomValues' on 'Crypto': The provided "
                        + "ArrayBufferView is not an integer-typed array.", "TypeMismatchError");
                }
                const byteLength = view.byteLength;
                if (byteLength > 65536) {
                    throw new DOMException(
                        "Failed to execute 'getRandomValues' on 'Crypto': The ArrayBufferView's "
                        + "byte length exceeds the number of bytes of entropy available.",
                        "QuotaExceededError");
                }
                const bytes = nativeRandomBytes(byteLength);
                const dest = new Uint8Array(view.buffer, view.byteOffset, byteLength);
                for (let i = 0; i < byteLength; i++) dest[i] = bytes[i];
                return view;
            };
            const hex = [];
            for (let i = 0; i < 256; i++) hex.push((i < 16 ? "0" : "") + i.toString(16));
            const randomUUID = function randomUUID() {
                const b = nativeRandomBytes(16);
                b[6] = (b[6] & 0x0f) | 0x40; // version 4
                b[8] = (b[8] & 0x3f) | 0x80; // variant 10xx
                return hex[b[0]] + hex[b[1]] + hex[b[2]] + hex[b[3]] + "-"
                    + hex[b[4]] + hex[b[5]] + "-" + hex[b[6]] + hex[b[7]] + "-"
                    + hex[b[8]] + hex[b[9]] + "-"
                    + hex[b[10]] + hex[b[11]] + hex[b[12]] + hex[b[13]] + hex[b[14]] + hex[b[15]];
            };
            const cryptoObj = objectCreate(ObjectProto);
            def(cryptoObj, "getRandomValues", getRandomValues);
            def(cryptoObj, "randomUUID", randomUUID);
            reflectDefineProperty(cryptoObj, Symbol.toStringTag,
                { value: "Crypto", writable: false, enumerable: false, configurable: true });
            reflectDefineProperty(globalThis, "crypto", {
                value: cryptoObj, writable: false, enumerable: true, configurable: true,
            });
        }

        // === TextEncoder / TextDecoder (UTF-8 only) ===
        if (globalThis.TextEncoder === undefined) {
            class TextEncoder {
                get encoding() { return "utf-8"; }
                encode(input) {
                    const str = input === undefined ? "" : StringCtor(input);
                    const out = [];
                    for (let i = 0; i < str.length; i++) {
                        let cp = str.charCodeAt(i);
                        if (cp >= 0xD800 && cp <= 0xDBFF) {
                            const lo = i + 1 < str.length ? str.charCodeAt(i + 1) : 0;
                            if (lo >= 0xDC00 && lo <= 0xDFFF) {
                                cp = 0x10000 + ((cp - 0xD800) << 10) + (lo - 0xDC00);
                                i++;
                            } else { cp = 0xFFFD; }
                        } else if (cp >= 0xDC00 && cp <= 0xDFFF) {
                            cp = 0xFFFD;
                        }
                        if (cp < 0x80) {
                            out.push(cp);
                        } else if (cp < 0x800) {
                            out.push(0xC0 | (cp >> 6), 0x80 | (cp & 0x3F));
                        } else if (cp < 0x10000) {
                            out.push(0xE0 | (cp >> 12), 0x80 | ((cp >> 6) & 0x3F), 0x80 | (cp & 0x3F));
                        } else {
                            out.push(0xF0 | (cp >> 18), 0x80 | ((cp >> 12) & 0x3F),
                                0x80 | ((cp >> 6) & 0x3F), 0x80 | (cp & 0x3F));
                        }
                    }
                    return Uint8Array.from(out);
                }
                encodeInto(source, dest) {
                    const bytes = this.encode(source);
                    const n = Math.min(bytes.length, dest.length);
                    for (let i = 0; i < n; i++) dest[i] = bytes[i];
                    // `read` (code units consumed) ≈ written when it all fits; the
                    // partial-fill code-unit accounting is approximate (v1).
                    return objectFreeze({ read: source === undefined ? 0 : StringCtor(source).length, written: n });
                }
            }
            reflectDefineProperty(TextEncoder.prototype, Symbol.toStringTag,
                { value: "TextEncoder", writable: false, enumerable: false, configurable: true });
            defGlobal("TextEncoder", TextEncoder);
        }
        if (globalThis.TextDecoder === undefined) {
            const toBytes = (input) => {
                if (input === undefined) return new Uint8Array(0);
                if (input instanceof ArrayBufferCtor) return new Uint8Array(input);
                if (abIsView(input)) return new Uint8Array(input.buffer, input.byteOffset, input.byteLength);
                throw new TypeError("TextDecoder.decode: input is not a BufferSource");
            };
            class TextDecoder {
                #fatal; #ignoreBOM; #label;
                constructor(label, options) {
                    this.#label = label === undefined ? "utf-8" : StringCtor(label).toLowerCase().trim();
                    if (this.#label !== "utf-8" && this.#label !== "utf8" && this.#label !== "unicode-1-1-utf-8") {
                        throw new RangeError("TextDecoder: only utf-8 is supported");
                    }
                    this.#fatal = !!(options && options.fatal);
                    this.#ignoreBOM = !!(options && options.ignoreBOM);
                }
                get encoding() { return "utf-8"; }
                get fatal() { return this.#fatal; }
                get ignoreBOM() { return this.#ignoreBOM; }
                decode(input) {
                    const bytes = toBytes(input);
                    let out = "";
                    let i = 0;
                    const n = bytes.length;
                    while (i < n) {
                        const b0 = bytes[i];
                        let cp, len;
                        if (b0 < 0x80) { cp = b0; len = 1; }
                        else if ((b0 & 0xE0) === 0xC0) { cp = b0 & 0x1F; len = 2; }
                        else if ((b0 & 0xF0) === 0xE0) { cp = b0 & 0x0F; len = 3; }
                        else if ((b0 & 0xF8) === 0xF0) { cp = b0 & 0x07; len = 4; }
                        else { if (this.#fatal) throw new TypeError("TextDecoder: invalid UTF-8"); out += "�"; i++; continue; }
                        if (i + len > n) { if (this.#fatal) throw new TypeError("TextDecoder: truncated UTF-8"); out += "�"; break; }
                        let ok = true;
                        for (let k = 1; k < len; k++) {
                            const bk = bytes[i + k];
                            if ((bk & 0xC0) !== 0x80) { ok = false; break; }
                            cp = (cp << 6) | (bk & 0x3F);
                        }
                        if (!ok || cp > 0x10FFFF || (cp >= 0xD800 && cp <= 0xDFFF)
                            || (len === 2 && cp < 0x80) || (len === 3 && cp < 0x800)
                            || (len === 4 && cp < 0x10000)) {
                            if (this.#fatal) throw new TypeError("TextDecoder: malformed UTF-8");
                            out += "�"; i++; continue;
                        }
                        out += String.fromCodePoint(cp);
                        i += len;
                    }
                    if (!this.#ignoreBOM && out.charCodeAt(0) === 0xFEFF) out = out.slice(1);
                    return out;
                }
            }
            reflectDefineProperty(TextDecoder.prototype, Symbol.toStringTag,
                { value: "TextDecoder", writable: false, enumerable: false, configurable: true });
            defGlobal("TextDecoder", TextDecoder);
        }

        // === requestIdleCallback / cancelIdleCallback ===
        // Shim over `setTimeout`; the deadline reports a fixed 50 ms budget.
        if (globalThis.requestIdleCallback === undefined) {
            const idleTimers = new MapCtor();
            let idleHandle = 0;
            defGlobal("requestIdleCallback", function requestIdleCallback(callback, options) {
                if (typeof callback !== "function") {
                    throw new TypeError("requestIdleCallback: callback is not a function");
                }
                const id = ++idleHandle;
                const start = performance.now();
                const timeout = options && typeof options.timeout === "number" ? options.timeout : 0;
                const timerId = globalThis.setTimeout(() => {
                    mapDelete.call(idleTimers, id);
                    const didTimeout = timeout > 0 && (performance.now() - start) >= timeout;
                    callback(objectFreeze({
                        didTimeout,
                        timeRemaining() { return Math.max(0, 50 - (performance.now() - start)); },
                    }));
                }, 1);
                mapSet.call(idleTimers, id, timerId);
                return id;
            });
            defGlobal("cancelIdleCallback", function cancelIdleCallback(id) {
                const timerId = mapGet.call(idleTimers, id);
                if (timerId !== undefined) {
                    globalThis.clearTimeout(timerId);
                    mapDelete.call(idleTimers, id);
                }
            });
        }

        // === Web Storage (Storage / localStorage / sessionStorage) ===
        // In-memory, per page (no persistence in a headless engine). The members
        // live on `Storage.prototype`, exactly as in browsers: script brand-checks
        // with `localStorage instanceof Storage` (VueUse's `useStorage` does, and
        // a bare `localStorage` object makes it throw a ReferenceError) and
        // analytics libraries monkey-patch `Storage.prototype.setItem`, so
        // per-instance closures would not do. The named-property surface
        // (`s.foo`, `delete s.foo`, `Object.keys(s)`) comes from a Proxy over a
        // backing Map. `Storage` has no `[LegacyOverrideBuiltIns]`, so anything on
        // the prototype chain wins over a stored key of the same name —
        // `localStorage.length` is the item count even after `setItem("length", …)`.
        if (globalThis.Storage === undefined) {
            const TOKEN = {};
            // Keyed by both the Proxy and its target, so the members work whether
            // `this` arrives as `localStorage` (the Proxy) or as the raw instance
            // (`Storage.prototype.getItem.call(target)`).
            const backing = new WeakMapCtor();
            const storeOf = (receiver) => {
                const store = weakMapGet.call(backing, receiver);
                if (store === undefined) throw new TypeError("Illegal invocation");
                return store;
            };
            class Storage {
                constructor(token) {
                    if (token !== TOKEN) throw new TypeError("Illegal constructor");
                }
                get length() { return storeOf(this).size; }
                key(index) {
                    const keys = arrayFrom(storeOf(this).keys());
                    const k = keys[index >>> 0];
                    return k === undefined ? null : k;
                }
                getItem(key) {
                    const v = mapGet.call(storeOf(this), StringCtor(key));
                    return v === undefined ? null : v;
                }
                setItem(key, value) {
                    mapSet.call(storeOf(this), StringCtor(key), StringCtor(value));
                }
                removeItem(key) { mapDelete.call(storeOf(this), StringCtor(key)); }
                clear() { storeOf(this).clear(); }
            }
            reflectDefineProperty(Storage.prototype, Symbol.toStringTag,
                { value: "Storage", writable: false, enumerable: false, configurable: true });
            defGlobal("Storage", Storage);

            const makeStorage = () => {
                const store = new MapCtor();
                const target = new Storage(TOKEN);
                const proxy = new Proxy(target, {
                    get(t, prop, receiver) {
                        if (typeof prop === "symbol" || reflectHas(t, prop)) {
                            return reflectGet(t, prop, receiver);
                        }
                        const v = mapGet.call(store, prop);
                        return v === undefined ? undefined : v;
                    },
                    set(t, prop, value) {
                        // A member name is never shadowed by a stored key.
                        if (typeof prop === "symbol" || reflectHas(t, prop)) return true;
                        mapSet.call(store, StringCtor(prop), StringCtor(value));
                        return true;
                    },
                    has(t, prop) {
                        if (typeof prop === "symbol" || reflectHas(t, prop)) return true;
                        return mapHas.call(store, prop);
                    },
                    deleteProperty(t, prop) {
                        if (typeof prop !== "symbol") mapDelete.call(store, StringCtor(prop));
                        return true;
                    },
                    ownKeys() { return arrayFrom(store.keys()); },
                    getOwnPropertyDescriptor(t, prop) {
                        if (typeof prop !== "symbol" && mapHas.call(store, prop)) {
                            return { value: mapGet.call(store, prop), writable: true, enumerable: true, configurable: true };
                        }
                        return reflectGetOwnPropertyDescriptor(t, prop);
                    },
                });
                weakMapSet.call(backing, target, store);
                weakMapSet.call(backing, proxy, store);
                return proxy;
            };
            for (const name of ["localStorage", "sessionStorage"]) {
                if (globalThis[name] === undefined) {
                    reflectDefineProperty(globalThis, name, {
                        value: makeStorage(), writable: false, enumerable: true, configurable: true,
                    });
                }
            }
        }

        // === StorageEvent ===
        // Constructible by script — VueUse mints one on every `useStorage` write —
        // which is the whole reason it exists here. The engine never *fires* one:
        // a storage event notifies the *other* documents of the origin, and a
        // headless page has none, so there is no one to deliver it to.
        if (globalThis.StorageEvent === undefined && typeof globalThis.Event === "function") {
            const orNull = (v) => v === undefined || v === null ? null : StringCtor(v);
            class StorageEvent extends globalThis.Event {
                #key; #oldValue; #newValue; #url; #storageArea;
                constructor(type, init) {
                    super(type, init);
                    const d = init === undefined || init === null ? {} : init;
                    this.#key = orNull(d.key);
                    this.#oldValue = orNull(d.oldValue);
                    this.#newValue = orNull(d.newValue);
                    this.#url = d.url === undefined || d.url === null ? "" : StringCtor(d.url);
                    this.#storageArea = d.storageArea === undefined ? null : d.storageArea;
                }
                get key() { return this.#key; }
                get oldValue() { return this.#oldValue; }
                get newValue() { return this.#newValue; }
                get url() { return this.#url; }
                get storageArea() { return this.#storageArea; }
            }
            reflectDefineProperty(StorageEvent.prototype, Symbol.toStringTag,
                { value: "StorageEvent", writable: false, enumerable: false, configurable: true });
            defGlobal("StorageEvent", StorageEvent);
        }

        // === NodeIterator / TreeWalker ===
        // Pure-JS DOM traversal over the existing Node navigation accessors,
        // required by Angular hydration (`createNodeIterator` over SHOW_COMMENT
        // markers). Direct construction is illegal (WebIDL: no constructor); the
        // guard token lets only `createNodeIterator`/`createTreeWalker` build one.
        if (globalThis.NodeIterator === undefined && globalThis.Document !== undefined) {
            const TOKEN = {};
            const FILTER_ACCEPT = 1, FILTER_REJECT = 2, FILTER_SKIP = 3;
            const showBit = (nodeType) => (nodeType >= 1 && nodeType <= 32) ? (1 << (nodeType - 1)) : 0;
            const normalizeWhatToShow = (v) => v === undefined ? 0xFFFFFFFF : (v >>> 0);
            const normalizeFilter = (f) => {
                if (f === undefined || f === null) return null;
                if (typeof f === "function" || typeof f === "object") return f;
                return null;
            };
            // `1 << (nodeType-1) & whatToShow`; `whatToShow` is masked to 32 bits.
            const bitMatch = (whatToShow, nodeType) => (showBit(nodeType) & whatToShow) !== 0;

            class NodeIterator {
                #root; #ref; #before; #show; #filter; #active = false;
                constructor(token, root, whatToShow, filter) {
                    if (token !== TOKEN) throw new TypeError("Illegal constructor");
                    this.#root = root; this.#ref = root; this.#before = true;
                    this.#show = whatToShow; this.#filter = filter;
                }
                get root() { return this.#root; }
                get referenceNode() { return this.#ref; }
                get pointerBeforeReferenceNode() { return this.#before; }
                get whatToShow() { return this.#show; }
                get filter() { return this.#filter; }
                #accept(node) {
                    if (!bitMatch(this.#show, node.nodeType)) return FILTER_SKIP;
                    if (this.#filter === null) return FILTER_ACCEPT;
                    if (this.#active) {
                        throw new DOMException("NodeIterator filter is already running", "InvalidStateError");
                    }
                    this.#active = true;
                    try {
                        return typeof this.#filter === "function"
                            ? this.#filter(node) : this.#filter.acceptNode(node);
                    } finally { this.#active = false; }
                }
                #following(node) {
                    if (node.firstChild) return node.firstChild;
                    let n = node;
                    while (n) {
                        if (n === this.#root) return null;
                        if (n.nextSibling) return n.nextSibling;
                        n = n.parentNode;
                    }
                    return null;
                }
                #preceding(node) {
                    if (node === this.#root) return null;
                    if (node.previousSibling) {
                        let n = node.previousSibling;
                        while (n.lastChild) n = n.lastChild;
                        return n;
                    }
                    return node.parentNode;
                }
                #traverse(next) {
                    let node = this.#ref;
                    let before = this.#before;
                    for (;;) {
                        if (next) {
                            if (!before) { node = this.#following(node); if (node === null) return null; }
                            before = false;
                        } else {
                            if (before) { node = this.#preceding(node); if (node === null) return null; }
                            before = true;
                        }
                        if (this.#accept(node) === FILTER_ACCEPT) break;
                    }
                    this.#ref = node; this.#before = before;
                    return node;
                }
                nextNode() { return this.#traverse(true); }
                previousNode() { return this.#traverse(false); }
                detach() {}
            }

            class TreeWalker {
                #root; #current; #show; #filter; #active = false;
                constructor(token, root, whatToShow, filter) {
                    if (token !== TOKEN) throw new TypeError("Illegal constructor");
                    this.#root = root; this.#current = root;
                    this.#show = whatToShow; this.#filter = filter;
                }
                get root() { return this.#root; }
                get whatToShow() { return this.#show; }
                get filter() { return this.#filter; }
                get currentNode() { return this.#current; }
                set currentNode(node) {
                    if (node === null || node === undefined) throw new TypeError("currentNode cannot be null");
                    this.#current = node;
                }
                #accept(node) {
                    if (!bitMatch(this.#show, node.nodeType)) return FILTER_SKIP;
                    if (this.#filter === null) return FILTER_ACCEPT;
                    if (this.#active) {
                        throw new DOMException("TreeWalker filter is already running", "InvalidStateError");
                    }
                    this.#active = true;
                    try {
                        return typeof this.#filter === "function"
                            ? this.#filter(node) : this.#filter.acceptNode(node);
                    } finally { this.#active = false; }
                }
                parentNode() {
                    let node = this.#current;
                    while (node !== null && node !== this.#root) {
                        node = node.parentNode;
                        if (node !== null && this.#accept(node) === FILTER_ACCEPT) {
                            this.#current = node; return node;
                        }
                    }
                    return null;
                }
                #child(first) {
                    let node = first ? this.#current.firstChild : this.#current.lastChild;
                    while (node !== null) {
                        const result = this.#accept(node);
                        if (result === FILTER_ACCEPT) { this.#current = node; return node; }
                        if (result === FILTER_SKIP) {
                            const child = first ? node.firstChild : node.lastChild;
                            if (child !== null) { node = child; continue; }
                        }
                        for (;;) {
                            const sibling = first ? node.nextSibling : node.previousSibling;
                            if (sibling !== null) { node = sibling; break; }
                            const parent = node.parentNode;
                            if (parent === null || parent === this.#root || parent === this.#current) return null;
                            node = parent;
                        }
                    }
                    return null;
                }
                firstChild() { return this.#child(true); }
                lastChild() { return this.#child(false); }
                #sibling(next) {
                    let node = this.#current;
                    if (node === this.#root) return null;
                    for (;;) {
                        let sibling = next ? node.nextSibling : node.previousSibling;
                        while (sibling !== null) {
                            node = sibling;
                            const result = this.#accept(node);
                            if (result === FILTER_ACCEPT) { this.#current = node; return node; }
                            sibling = next ? node.firstChild : node.lastChild;
                            if (result === FILTER_REJECT || sibling === null) {
                                sibling = next ? node.nextSibling : node.previousSibling;
                            }
                        }
                        node = node.parentNode;
                        if (node === null || node === this.#root) return null;
                        if (this.#accept(node) === FILTER_ACCEPT) return null;
                    }
                }
                nextSibling() { return this.#sibling(true); }
                previousSibling() { return this.#sibling(false); }
                nextNode() {
                    let node = this.#current;
                    let result = FILTER_ACCEPT;
                    for (;;) {
                        while (result !== FILTER_REJECT && node.firstChild !== null) {
                            node = node.firstChild;
                            result = this.#accept(node);
                            if (result === FILTER_ACCEPT) { this.#current = node; return node; }
                        }
                        let temporary = node;
                        let sibling = null;
                        while (temporary !== null) {
                            if (temporary === this.#root) return null;
                            sibling = temporary.nextSibling;
                            if (sibling !== null) { node = sibling; break; }
                            temporary = temporary.parentNode;
                        }
                        if (temporary === null) return null;
                        result = this.#accept(node);
                        if (result === FILTER_ACCEPT) { this.#current = node; return node; }
                    }
                }
                previousNode() {
                    let node = this.#current;
                    while (node !== this.#root) {
                        let sibling = node.previousSibling;
                        while (sibling !== null) {
                            node = sibling;
                            let result = this.#accept(node);
                            while (result !== FILTER_REJECT && node.lastChild !== null) {
                                node = node.lastChild;
                                result = this.#accept(node);
                            }
                            if (result === FILTER_ACCEPT) { this.#current = node; return node; }
                            sibling = node.previousSibling;
                        }
                        if (node === this.#root || node.parentNode === null) return null;
                        node = node.parentNode;
                        if (this.#accept(node) === FILTER_ACCEPT) { this.#current = node; return node; }
                    }
                    return null;
                }
            }

            for (const [Ctor, tag] of [[NodeIterator, "NodeIterator"], [TreeWalker, "TreeWalker"]]) {
                reflectDefineProperty(Ctor.prototype, Symbol.toStringTag,
                    { value: tag, writable: false, enumerable: false, configurable: true });
                defGlobal(tag, Ctor);
            }

            const docProto = globalThis.Document.prototype;
            def(docProto, "createNodeIterator", function createNodeIterator(root, whatToShow, filter) {
                if (root === undefined || root === null) {
                    throw new TypeError("createNodeIterator: root is not a Node");
                }
                return new NodeIterator(TOKEN, root, normalizeWhatToShow(whatToShow), normalizeFilter(filter));
            });
            def(docProto, "createTreeWalker", function createTreeWalker(root, whatToShow, filter) {
                if (root === undefined || root === null) {
                    throw new TypeError("createTreeWalker: root is not a Node");
                }
                return new TreeWalker(TOKEN, root, normalizeWhatToShow(whatToShow), normalizeFilter(filter));
            });
        }

        // === Response.body (minimal ReadableStream) ===
        // Angular's `FetchBackend` reads a response through
        // `response.body.getReader()`; without `body` the reader path yields an
        // empty body, so every runtime `fetch` (Material icon sets, client-side
        // data) comes back blank (SSR hydration survives only because its data
        // rides `TransferState`, not the fetch body). This is a one-shot byte
        // stream backed by `arrayBuffer()` — the whole body arrives in a single
        // `read()`, which is all the fetch backends need. A minimal
        // `ReadableStream` global is exposed for feature detection.
        if (globalThis.ReadableStream === undefined) {
            const makeReader = (pull) => {
                let done = false;
                let started = null;
                return {
                    read() {
                        if (done) return globalThis.Promise.resolve({ value: undefined, done: true });
                        if (started === null) started = globalThis.Promise.resolve(pull());
                        return started.then((chunk) => {
                            done = true;
                            return chunk && chunk.length
                                ? { value: chunk, done: false }
                                : { value: undefined, done: true };
                        });
                    },
                    releaseLock() {},
                    cancel() { done = true; return globalThis.Promise.resolve(); },
                    get closed() { return globalThis.Promise.resolve(); },
                };
            };
            // A byte stream over a lazily-produced Uint8Array (`pull` returns it
            // or a promise of it).
            const makeByteStream = (pull) => {
                let locked = false;
                const stream = objectCreate(ReadableStreamProto);
                def(stream, "getReader", function getReader() {
                    locked = true;
                    return makeReader(pull);
                });
                def(stream, "cancel", function cancel() { return globalThis.Promise.resolve(); });
                reflectDefineProperty(stream, "locked", {
                    get() { return locked; }, enumerable: true, configurable: true,
                });
                return stream;
            };
            // A bare `ReadableStream` constructor: enough for `typeof` /
            // `instanceof` feature detection and simple `{ start(controller) }`
            // sources that enqueue then close (no backpressure, one reader).
            function ReadableStream(source) {
                const chunks = [];
                let closed = false;
                const controller = {
                    enqueue(chunk) { chunks.push(chunk); },
                    close() { closed = true; },
                    error() { closed = true; },
                };
                if (source && typeof source.start === "function") {
                    try { source.start(controller); } catch (_e) { /* ignore */ }
                }
                let index = 0;
                let locked = false;
                reflectDefineProperty(this, "locked", {
                    get() { return locked; }, enumerable: true, configurable: true,
                });
                def(this, "getReader", function getReader() {
                    locked = true;
                    return {
                        read() {
                            if (index < chunks.length) {
                                return globalThis.Promise.resolve({ value: chunks[index++], done: false });
                            }
                            return globalThis.Promise.resolve({ value: undefined, done: true });
                        },
                        releaseLock() { locked = false; },
                        cancel() { index = chunks.length; return globalThis.Promise.resolve(); },
                        get closed() { return globalThis.Promise.resolve(); },
                    };
                });
                def(this, "cancel", function cancel() { index = chunks.length; return globalThis.Promise.resolve(); });
            }
            const ReadableStreamProto = ReadableStream.prototype;
            reflectDefineProperty(ReadableStreamProto, Symbol.toStringTag,
                { value: "ReadableStream", writable: false, enumerable: false, configurable: true });
            defGlobal("ReadableStream", ReadableStream);

            if (globalThis.Response !== undefined
                && reflectGetOwnPropertyDescriptor(globalThis.Response.prototype, "body") === undefined) {
                reflectDefineProperty(globalThis.Response.prototype, "body", {
                    get() {
                        const response = this;
                        return makeByteStream(() =>
                            response.arrayBuffer().then((ab) => new Uint8Array(ab)));
                    },
                    enumerable: true, configurable: true,
                });
            }
        }

        // The `__oxide_*` helpers have been captured into locals; hide them.
        reflectDeleteProperty(globalThis, "__oxide_randomBytes");
    };

    // URLSearchParams pair iteration, built on a native `snapshot(params)`
    // that returns the current `[[name, value], …]` list.
    const installParamsIterable = (proto, snapshot) => {
        const entries = function () { return snapshot(this)[Symbol.iterator](); };
        const keys = function () { return snapshot(this).map((e) => e[0])[Symbol.iterator](); };
        const values = function () { return snapshot(this).map((e) => e[1])[Symbol.iterator](); };
        const forEach = function (cb, thisArg) {
            if (typeof cb !== "function") {
                throw new TypeError("URLSearchParams.forEach: callback is not a function");
            }
            for (const [k, v] of snapshot(this)) cb.call(thisArg, v, k, this);
        };
        const desc = (value) => ({ value, writable: true, enumerable: true, configurable: true });
        Object.defineProperty(proto, "entries", desc(entries));
        Object.defineProperty(proto, "keys", desc(keys));
        Object.defineProperty(proto, "values", desc(values));
        Object.defineProperty(proto, "forEach", desc(forEach));
        Object.defineProperty(proto, Symbol.iterator, {
            value: entries, writable: true, enumerable: false, configurable: true,
        });
    };

    return {
        newWrapperMap, cacheGet, cacheSet,
        collectionProxy, installIterable, installValueIterator, adoptedSheetsProxy,
        setToStringTag, makeDomException, structuredClone,
        makePromise, resolvedPromise, recordPairs, installParamsIterable,
        freeze, initStyleProps, styleProxy, datasetProxy, deleteProperty,
        ceConstruct, installLateGlobals, enqueueMicrotask,
        objectPrototype: Object.prototype,
    };
})()
