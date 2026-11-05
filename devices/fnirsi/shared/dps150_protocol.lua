-- Driver for the FNIRSI DPS-150 protocol family (voltage_source), used by the DPS-150 and the
-- IPS3608 (one PC application for both). See ../README.md for sources and known gaps.
--
-- Frame: dir, type, cmd, len, data..., checksum      checksum = (cmd + len + sum(data)) & 0xFF
--   dir   0xF1 host -> device, 0xF0 device -> host
--   type  0xA1 query/response, 0xB1 write, 0xC1 connect control
--   cmd   0x00 connect (data 1) / disconnect (data 0)
--         0xC1 set voltage, 0xC2 set current (float32 LE, no reply)
--         0xDB output on/off (data 1/0; echoed by the DPS-150, not by the IPS3608)
--         0xC3 telemetry pushed by the device: Vout, Iout, Pout as float32 LE
--         0xDE query: model name (ASCII), 0xFF query: full status (139 bytes)
--
-- params (from the device definition):
--   max_voltage, max_current   device maximums (desk refuses anything above)
--   telemetry_ms               how long read() waits for a measurement (default 1500)
--   settle_ms                  ignore measurements older than this after a command (default 300)
--   echo_ms                    how long set_output() waits for the echo (default 500)
--   wait                       how long open() waits for the port to appear (default "2s")
--   name                       expected model name (query 0xDE). If set, open() refuses other
--                              devices: the USB id is a generic Artery AT32 one shared by others
--   status                     byte offsets in the full status (query 0xFF), e.g.
--                              { voltage_set = 4, current_set = 8, voltage = 12, current = 16,
--                                power = 20, output = 107 }. With it, read() reports setpoints
--                              and output state; without it, only pushed measurements.
--   status_ms                  how long to wait for a query reply (default 700)
--   boot_ms                    right after mains on, the USB port appears before the device
--                              answers (writes fail, queries time out): open() keeps retrying
--                              this long (default 3000)

local HOST, DEVICE = 0xF1, 0xF0
local T_QUERY, T_WRITE, T_CONNECT = 0xA1, 0xB1, 0xC1
local C_CONNECT, C_SET_V, C_SET_I, C_OUTPUT, C_TELEMETRY = 0x00, 0xC1, 0xC2, 0xDB, 0xC3
local C_NAME, C_STATUS = 0xDE, 0xFF
local MAX_LEN = 160 -- longest known frame (full status) is 139 bytes of data

local port        -- open during a session
local port_path   -- for messages
local rx = ""     -- received, not yet parsed
local output_echo -- output state the device confirmed in this session (nil = unknown)
local last_command = -math.huge -- ctx.clock() of the last command sent

local function checksum(cmd, data)
  local sum = cmd + #data
  for i = 1, #data do
    sum = sum + data:byte(i)
  end
  return sum & 0xFF
end

local function write_frame(kind, cmd, data)
  port:write(string.char(HOST, kind, cmd, #data) .. data .. string.char(checksum(cmd, data)))
end

-- A command that changes the device (measurements before it are stale).
local function send(ctx, kind, cmd, data)
  write_frame(kind, cmd, data)
  last_command = ctx.clock()
end

-- Next complete, valid frame from the device, or nil. Skips garbage and resynchronizes.
local function parse()
  while true do
    local start = rx:find(string.char(DEVICE), 1, true)
    if not start then
      rx = ""
      return nil
    end
    rx = rx:sub(start)
    if #rx < 4 then
      return nil
    end
    local kind, cmd, len = rx:byte(2, 4)
    if len > MAX_LEN then
      rx = rx:sub(2) -- not a real frame start
    elseif #rx < 5 + len then
      return nil -- wait for the rest
    else
      local data = rx:sub(5, 4 + len)
      if rx:byte(5 + len) == checksum(cmd, data) then
        rx = rx:sub(6 + len)
        return { kind = kind, cmd = cmd, data = data }
      end
      rx = rx:sub(2)
    end
  end
end

-- Wait up to `ms` for a frame `cmd` that satisfies `accept` (default: any), noting output echoes
-- on the way.
local function wait_for(ctx, cmd, ms, accept)
  local deadline = ctx.clock() + ms
  while true do
    local f = parse()
    while f do
      if f.cmd == C_OUTPUT and #f.data >= 1 then
        output_echo = f.data:byte(1) == 1
      end
      if f.cmd == cmd and (accept == nil or accept(f)) then
        return f
      end
      f = parse()
    end
    local left = deadline - ctx.clock()
    if left <= 0 then
      return nil
    end
    rx = rx .. port:read(64, math.min(left, 50))
  end
end

-- Ask the device something; returns the reply frame or nil.
local function query(ctx, cmd, accept)
  write_frame(T_QUERY, cmd, "\0")
  return wait_for(ctx, cmd, ctx.params.status_ms or 700, accept)
end

-- Connect, and check the model name if the definition gives one. Returns nil when the device
-- doesn't answer (yet), raises when it's the wrong device.
local function connect(ctx)
  port = ctx.serial.open({ baud = 115200, wait = ctx.params.wait or "2s" })
  port_path = port.path
  rx, output_echo = "", nil
  port:clear()
  send(ctx, T_CONNECT, C_CONNECT, "\1")
  ctx.sleep(100)
  local expected = ctx.params.name
  if not expected then
    return true
  end
  local reply = query(ctx, C_NAME)
  if not reply then
    return nil
  end
  if reply.data ~= expected then
    close(ctx)
    error(string.format("the device on %s is %q, not a %s; nothing was sent", port_path, reply.data, expected), 0)
  end
  return true
end

function open(ctx)
  local deadline = ctx.clock() + (ctx.params.boot_ms or 3000)
  while true do
    local ok, result = pcall(connect, ctx)
    if ok and result then
      return
    end
    -- A wrong device fails at once; "not ready yet" (write failed, no answer) is retried.
    local why = ok and "it doesn't answer" or tostring(result)
    if ok == false and why:find("nothing was sent", 1, true) then
      error(result, 0)
    end
    if port then
      port:close()
      port = nil
    end
    if ctx.clock() >= deadline then
      if not ok then
        error(result, 0) -- re-raise as is: host errors (port missing, busy) keep their error code
      end
      local name = ctx.params.name and (" as a " .. ctx.params.name) or ""
      error(string.format("the device doesn't respond%s", name), 0)
    end
    ctx.sleep(250)
  end
end

function close(ctx)
  if port then
    -- Disconnecting releases the front panel, which is locked while connected.
    pcall(send, ctx, T_CONNECT, C_CONNECT, "\0")
    ctx.sleep(100)
    port:close()
    port = nil
  end
end

function info(ctx)
  return { max_voltage = ctx.params.max_voltage, max_current = ctx.params.max_current }
end

function set_voltage(ctx, volts)
  send(ctx, T_WRITE, C_SET_V, string.pack("<f", volts))
end

function set_current(ctx, amps)
  send(ctx, T_WRITE, C_SET_I, string.pack("<f", amps))
end

function set_output(ctx, on)
  output_echo = nil
  send(ctx, T_WRITE, C_OUTPUT, on and "\1" or "\0")
  -- The DPS-150 echoes this command; with the echo, read() can report the output state. With a
  -- status layout, read() asks the device instead (the IPS3608 doesn't echo).
  if not ctx.params.status then
    wait_for(ctx, C_OUTPUT, ctx.params.echo_ms or 500)
  end
end

-- Everything from the full status, using the definition's byte offsets.
local function read_status(ctx, layout)
  -- Let the device settle after our last command, then ask.
  local settle_until = last_command + (ctx.params.settle_ms or 300)
  if ctx.clock() < settle_until then
    ctx.sleep(settle_until - ctx.clock())
  end
  local reply = query(ctx, C_STATUS, function(f)
    return #f.data >= 24
  end)
  if not reply then
    error(string.format("no status reply from the device within %d ms", ctx.params.status_ms or 700))
  end
  local d = reply.data
  local function float_at(offset)
    if offset and #d >= offset + 4 then
      return (string.unpack("<f", d, offset + 1))
    end
  end
  local output = output_echo
  if layout.output and #d > layout.output then
    output = d:byte(layout.output + 1) == 1
  end
  return {
    output = output,
    voltage_set = float_at(layout.voltage_set),
    current_set = float_at(layout.current_set),
    voltage = float_at(layout.voltage),
    current = float_at(layout.current),
    power = float_at(layout.power),
  }
end

function read(ctx)
  if ctx.params.status then
    return read_status(ctx, ctx.params.status)
  end
  local wait = ctx.params.telemetry_ms or 1500
  local fresh_after = last_command + (ctx.params.settle_ms or 300)
  local t = wait_for(ctx, C_TELEMETRY, wait, function(f)
    return #f.data >= 12 and ctx.clock() >= fresh_after
  end)
  if not t then
    error(string.format("no measurement from the device within %d ms (is it on and connected?)", wait))
  end
  local v, i, p = string.unpack("<fff", t.data)
  -- The protocol has no known way to read setpoints back, so they're not reported: desk will
  -- show them as "not confirmed" instead of pretending.
  return { output = output_echo, voltage = v, current = i, power = p }
end
