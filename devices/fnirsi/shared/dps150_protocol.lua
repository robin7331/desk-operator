-- Driver for the FNIRSI DPS-150 protocol family (voltage_source), used by the DPS-150 and the
-- IPS3608 (one PC application for both). See ../README.md for sources and known gaps.
--
-- Frame: dir, type, cmd, len, data..., checksum      checksum = (cmd + len + sum(data)) & 0xFF
--   dir   0xF1 host -> device, 0xF0 device -> host
--   type  0xA1 query/response, 0xB1 write, 0xC1 connect control
--   cmd   0x00 connect (data 1) / disconnect (data 0)
--         0xC1 set voltage, 0xC2 set current (float32 LE, no reply)
--         0xDB output on/off (data 1/0, echoed by the device)
--         0xC3 telemetry pushed by the device: Vout, Iout, Pout as float32 LE
--
-- params (from the device definition):
--   max_voltage, max_current   device maximums (desk refuses anything above)
--   telemetry_ms               how long read() waits for a measurement (default 1500)
--   settle_ms                  ignore measurements older than this after a command (default 300)
--   echo_ms                    how long set_output() waits for the echo (default 500)
--   wait                       how long open() waits for the port to appear (default "2s")

local HOST, DEVICE = 0xF1, 0xF0
local T_WRITE, T_CONNECT = 0xB1, 0xC1
local C_CONNECT, C_SET_V, C_SET_I, C_OUTPUT, C_TELEMETRY = 0x00, 0xC1, 0xC2, 0xDB, 0xC3
local MAX_LEN = 160 -- longest known frame (full status) is 139 bytes of data

local port        -- open during a session
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

local function send(ctx, kind, cmd, data)
  port:write(string.char(HOST, kind, cmd, #data) .. data .. string.char(checksum(cmd, data)))
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

function open(ctx)
  port = ctx.serial.open({ baud = 115200, wait = ctx.params.wait or "2s" })
  rx, output_echo = "", nil
  port:clear()
  send(ctx, T_CONNECT, C_CONNECT, "\1")
  ctx.sleep(100)
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
  -- The device echoes this command; with the echo, read() can report the output state.
  wait_for(ctx, C_OUTPUT, ctx.params.echo_ms or 500)
end

function read(ctx)
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
