"""A read cache for the link logs the mocked games and the relay pass to each other (*.log), shared by every Lua state of the
test process (the games' and the relay's) and by the harness's Python side.

Why: on Windows, opening a file for reading right after it was written costs 1 to 7 ms (measured 2026-10-18 on the owner's PC:
append + read 4 to 7 ms, append + stat 0.15 ms; the file is scanned again). Every game reads to_game.log every frame and the relay
reads every from_game.log every pump, so the lupa suite spent most of its time in those opens (a two-game test: 2.0 s, 0.5 s with
the cache).

What stays real: every write goes to the file on disk as before, through a real handle of the same mode (a held handle stays
held). A read of a file that is no *.log, or whose size or modification time is not what the cache saw last, reads the disk. Only
the bytes of a *.log whose every write since went through this module (or that was read and has not changed since) are served from
memory, and they are the bytes the disk holds. Not modelled: a read handle does not keep a file from being deleted (on Windows a
real one does). The file layer itself is tested on real files (test_link_files.py's own Lua states have no cache);
FEVERLINK3_TEST_REAL_IO=1 turns the cache off for a whole run.
"""
import os

ENABLED = os.environ.get("FEVERLINK3_TEST_REAL_IO") != "1"


def _key(path):
    return os.path.normcase(os.path.abspath(path))


def _cacheable(path):
    return ENABLED and isinstance(path, str) and path.endswith(".log")


def _stat(path):
    try:
        st = os.stat(path)
    except OSError:
        return None
    return st.st_size, st.st_mtime_ns


class FileCache:
    def __init__(self):
        self.d = {}   # key -> [bytearray as the disk holds it, (size, mtime_ns) after the last write or read]

    def entry(self, path):
        """The file's bytes (a bytearray the cache owns: do not change it), None when it cannot be opened."""
        k = _key(path)
        st = _stat(path)
        if st is None:
            self.d.pop(k, None)
            return None
        e = self.d.get(k)
        if e is not None and e[1] == st:
            return e[0]
        try:
            with open(path, "rb") as f:
                data = bytearray(f.read())
        except OSError:
            self.d.pop(k, None)
            return None
        after = _stat(path)
        if after == st and after[0] == len(data):
            self.d[k] = [data, after]
        else:
            self.d.pop(k, None)
        return data

    def read(self, path):
        """The file's bytes, None when it cannot be opened."""
        if not _cacheable(path):
            try:
                with open(path, "rb") as f:
                    return f.read()
            except OSError:
                return None
        data = self.entry(path)
        return None if data is None else bytes(data)

    def before_write(self, path):
        """What a write handle needs to know at its open: the entry it may extend at its close (None: none)."""
        if not _cacheable(path):
            return None
        k = _key(path)
        st = _stat(path)
        if st is None:
            return (k, None, 0)   # a new file
        e = self.d.get(k)
        if e is not None and e[1] == st:
            return (k, e, st[0])
        return None

    def after_write(self, path, before, written, append):
        """A write handle closed after writing `written`: the cache holds the new content when it is sure of it."""
        if not _cacheable(path):
            return
        k = _key(path)
        st = _stat(path)
        e = self.d.get(k)
        if st is None:
            self.d.pop(k, None)
            return
        if not append:
            # "w": the file is what this handle wrote, if nobody else wrote meanwhile
            if st[0] == len(written):
                self.d[k] = [bytearray(written), st]
            else:
                self.d.pop(k, None)
            return
        if before is not None:
            _, was, size = before
            if was is None and e is None and st[0] == len(written):
                self.d[k] = [bytearray(written), st]   # the file did not exist at the open
                return
            if was is not None and e is was and len(was[0]) == size and st[0] == size + len(written):
                was[0].extend(written)
                was[1] = st
                return
        self.d.pop(k, None)

    def append(self, path, data):
        """The harness's own append (bytes), through a real handle."""
        before = self.before_write(path)
        with open(path, "ab") as f:
            f.write(data)
        self.after_write(path, before, data, True)

    def write(self, path, data):
        """The harness's own rewrite (bytes), through a real handle."""
        with open(path, "wb") as f:
            f.write(data)
        self.after_write(path, None, data, False)


CACHE = FileCache()


def read_text(path):
    """The file as text with universal newlines (what open(path, encoding="utf-8").read() gives), "" when it is missing."""
    data = CACHE.read(path)
    if data is None:
        return ""
    return data.decode("utf-8").replace("\r\n", "\n").replace("\r", "\n")


# The Lua side: io.open of the state is replaced. A read handle ("r", "rb") of a *.log is a table over the cache: its size and the
# bytes it reads are the file's now, as a real handle on a growing file sees them (seek, read, lines, close). Every other open is the
# real io.open; a write handle is the real handle, and its close tells the cache what was written.
LUA = r'''
local FASTIO_SIZE, FASTIO_SLICE, FASTIO_BEFORE, FASTIO_AFTER, real_open = ...
local function memfile(path)
  local h = { pos = 0 }
  function h:seek(whence, off)
    whence, off = whence or "cur", off or 0
    if whence == "end" then h.pos = (FASTIO_SIZE(path) or 0) + off
    elseif whence == "set" then h.pos = off
    else h.pos = h.pos + off end
    if h.pos < 0 then h.pos = 0 end
    return h.pos
  end
  local function one(fmt)
    if type(fmt) == "number" then
      local s = FASTIO_SLICE(path, h.pos, fmt)
      if s == nil then return nil end
      if fmt > 0 and #s == 0 then return nil end
      h.pos = h.pos + #s
      return s
    end
    fmt = tostring(fmt or "*l"):gsub("^%*", "")
    local c = fmt:sub(1, 1)
    if c == "a" then local s = FASTIO_SLICE(path, h.pos, -1) or ""; h.pos = h.pos + #s; return s end
    local rest = FASTIO_SLICE(path, h.pos, -1) or ""
    if c == "l" or c == "L" then
      if #rest == 0 then return nil end
      local i = rest:find("\n", 1, true)
      local s
      if i then s = rest:sub(1, c == "L" and i or i - 1); h.pos = h.pos + i
      else s = rest; h.pos = h.pos + #rest end
      return s
    end
    if c == "n" then
      local s, e = rest:find("^%s*[-+]?%d+%.?%d*[eE]?[-+]?%d*")
      if not s then return nil end
      h.pos = h.pos + e; return tonumber(rest:sub(s, e))
    end
    error("bad read format " .. tostring(fmt))
  end
  function h:read(...)
    local n = select("#", ...)
    if n == 0 then return one("*l") end
    local out = {}
    for i = 1, n do out[i] = one((select(i, ...))); if out[i] == nil then return table.unpack(out, 1, i) end end
    return table.unpack(out, 1, n)
  end
  function h:lines(fmt) return function() return one(fmt or "*l") end end
  function h:close() h.closed = true; return true end
  function h:flush() return h end
  function h:setvbuf() return true end
  return h
end
local function writefile(path, mode, f)
  local append = mode:sub(1, 1) == "a"
  local ticket = FASTIO_BEFORE(path)
  local parts = {}
  local h = {}
  function h:write(...)
    for i = 1, select("#", ...) do parts[#parts + 1] = tostring((select(i, ...))) end
    local ok, e = f:write(...)
    if ok == f then return h end
    return ok, e
  end
  function h:seek(...) return f:seek(...) end
  function h:read(...) return f:read(...) end
  function h:lines(...) return f:lines(...) end
  function h:flush() f:flush(); return h end
  function h:setvbuf(...) return f:setvbuf(...) end
  function h:close()
    local ok, e = f:close()
    FASTIO_AFTER(path, ticket, table.concat(parts), append)
    return ok, e
  end
  return h
end
io.open = function(path, mode)
  mode = mode or "r"
  local m = mode:gsub("b", "")
  if type(path) == "string" and path:sub(-4) == ".log" then
    if m == "r" then
      if FASTIO_SIZE(path) == nil then return nil, path .. ": No such file or directory", 2 end
      return memfile(path)
    elseif m == "a" or m == "w" then
      local f, e, c = real_open(path, mode)
      if not f then return f, e, c end
      return writefile(path, mode, f)
    end
  end
  return real_open(path, mode)
end
'''


# What a Lua state calls, handed to its chunk as plain functions (lupa hands Lua strings over as str, Python bytes come back as Lua
# strings). Never as attributes of a Python object: lupa's wrapper of a bound method looked up per call was seen calling another
# method with another call's arguments (2026-10-18: before_write given after_write's four arguments), and the mod's frame failed.
_pending, _ticket = {}, [0]


def _size(path):
    data = CACHE.entry(path)
    return None if data is None else len(data)


def _slice(path, pos, n):
    data = CACHE.entry(path)
    if data is None:
        return None
    pos, n = int(pos), int(n)
    return bytes(data[pos:] if n < 0 else data[pos:pos + n])


def _before_write(path):
    _ticket[0] += 1
    _pending[_ticket[0]] = CACHE.before_write(path)
    return _ticket[0]


def _after_write(path, ticket, written, append):
    before = _pending.pop(ticket, None)
    if isinstance(written, str):
        written = written.encode("utf-8")
    CACHE.after_write(path, before, written, bool(append))


def install(runtime):
    """Puts the cache under io.open of a Lua state and the bytecode cache under its LOAD_SRC (before the mod's files load)."""
    runtime.globals().FL3_BYTECODE = bytecode
    if not ENABLED:
        return
    runtime.execute(LUA, _size, _slice, _before_write, _after_write, runtime.globals().io.open)


# ---------------------------------------------------------------- the mod's files, compiled once per run
# A mocked game loads each of the mod's files (the bridge twice: one per half, and again at a reload): about 35 ms of every game's
# start was compiling them. The harness's LOAD_SRC (test_lockstep.HARNESS) loads the bytecode instead, into the same environment,
# with the same chunk name and debug information (string.dump keeps both). A file that does not compile is loaded from its text as
# before, so its error is the same. FEVERLINK3_TEST_NO_BYTECODE=1 turns this off.
import lupa.lua52 as _lupa  # noqa: E402

_DUMPER = _lupa.LuaRuntime(encoding=None)
_dump = _DUMPER.eval('function(src, name) local f = load(src, name, "t"); return f and string.dump(f) or false end')
_BYTECODE = {}
BYTECODE_ON = os.environ.get("FEVERLINK3_TEST_NO_BYTECODE") != "1"


def bytecode(src, name):
    if not BYTECODE_ON or not isinstance(src, str) or not isinstance(name, str):
        return None
    key = (name, src)
    bc = _BYTECODE.get(key)
    if bc is None:
        bc = _dump(src.encode("utf-8"), name.encode("utf-8")) or False
        _BYTECODE[key] = bc
    return bc or None


LOAD_SRC_LUA = r'''
function LOAD_SRC(src, name, mode, env)
  local bc = FL3_BYTECODE and FL3_BYTECODE(src, name)
  if bc then return load(bc, name, "b", env) end
  return load(src, name, mode, env)
end
'''
