-- OWON OEL1515 DC electronic load: SCPI text over a USB serial adapter. Notes: ../README.md.
-- Only *IDN? answers in local mode, so every session goes to remote mode, which locks the front
-- panel, and back to local at its end. Only the commands below are ever sent.

PARAMS = {
  model    = { type = "string", default = "OEL1515", doc = "model name expected in *IDN?; refuse other devices" },
  baud     = { type = "integer", default = 115200, doc = "the load's baud rate (System Setup menu)" },
  wait     = { type = "string", default = "3s", doc = "how long to wait for the port to appear" },
  reply_ms = { type = "number", default = 500, doc = "wait for one reply" },
  attempts = { type = "integer", default = 2, doc = "how often a query is sent before giving up" },
  gap_ms   = { type = "number", default = 100, doc = "quiet time after a command, which gets no reply" },
}

local FUNCTIONS = {
  current = "CURR",
  voltage = "VOLT",
  resistance = "RES",
  power = "POW",
  dynamic = "DYN",
}

-- A setpoint only takes effect in the function of the same name.
local SETPOINTS = {
  current = { command = "CURR", format = "%.3f" },
  voltage = { command = "VOLT", format = "%.3f" },
  resistance = { command = "RES", format = "%.2f" },
  power = { command = "POW", format = "%.3f" },
}

local port
local firmware
local in_remote = false

local function trim(s)
  return (s:gsub("^[%c%s]+", ""):gsub("[%c%s]+$", ""))
end

-- Send a query until a reply passes `parse` (returning non-nil). nil if none does. In local mode
-- the load answers queries with an empty line, and unknown ones with ERR.
local function query(ctx, command, parse)
  for _ = 1, ctx.params.attempts do
    port:clear()
    port:write(command .. "\n")
    local deadline = ctx.clock() + ctx.params.reply_ms
    local buffer = ""
    while true do
      local left = deadline - ctx.clock()
      if left <= 0 then
        break
      end
      -- One byte: read() waits out its whole timeout unless it gets all it asked for.
      buffer = buffer .. port:read(1, math.ceil(left))
      for line in buffer:gmatch("([^\n]*)\n") do
        local parsed = parse(trim(line))
        if parsed ~= nil then
          return parsed
        end
      end
      buffer = buffer:match("([^\n]*)$")
    end
  end
  return nil
end

local function command(ctx, text)
  port:write(text .. "\n")
  ctx.sleep(ctx.params.gap_ms)
end

-- The manual says 0|1, real units answer ON|OFF.
local function parse_bool(line)
  local upper = line:upper()
  if upper == "ON" or upper == "1" then
    return true
  elseif upper == "OFF" or upper == "0" then
    return false
  end
  return nil
end

local function parse_function(line)
  local lower = line:lower()
  for _, known in ipairs { "current", "voltage", "resistance", "power", "dynamic" } do
    if lower == known then
      return known
    end
  end
  return nil
end

-- voltage,current,power,OVP,OCP,OPP
local function parse_info(line)
  local v, i, p, ovp, ocp, opp = line:match("^([^,]+),([^,]+),([^,]+),([^,]+),([^,]+),([^,]+)$")
  if not v then
    return nil
  end
  local info = {
    voltage = tonumber(v),
    current = tonumber(i),
    power = tonumber(p),
    ovp = parse_bool(ovp),
    ocp = parse_bool(ocp),
    opp = parse_bool(opp),
  }
  if not (info.voltage and info.current and info.power) or info.ovp == nil or info.ocp == nil or info.opp == nil then
    return nil
  end
  return info
end

function open(ctx)
  port = ctx.serial.open { baud = ctx.params.baud, wait = ctx.params.wait }
  local idn = query(ctx, "*IDN?", function(line)
    return line:match("^OWON,") and line or nil
  end)
  if not idn then
    error("no answer to *IDN? from an OWON load: not switched on, still booting, wrong port, wrong baud rate, or COMM PRO in the System Setup menu isn't SCPI")
  end
  local model, version = idn:match("^OWON,([^,]*),[^,]*,([^,]*)")
  -- The T version only adds test functions: same ratings.
  if model ~= ctx.params.model and model ~= ctx.params.model .. "T" then
    error("expected an OWON " .. ctx.params.model .. ", found: " .. idn)
  end
  firmware = version and version:gsub("^FV:", "") or nil
  command(ctx, "SYST:REM")
  in_remote = true
end

function close(ctx)
  if in_remote then
    command(ctx, "SYST:LOC")
    in_remote = false
  end
end

function read(ctx)
  local info = query(ctx, "MEAS:ALL:INFO?", parse_info)
  if not info then
    error("no valid answer to MEAS:ALL:INFO?: the load may have gone back to local mode (SHIFT + On/Off on its front panel)")
  end
  local result = {
    firmware = firmware,
    measured_voltage = info.voltage,
    measured_current = info.current,
    measured_power = info.power,
    ovp_tripped = info.ovp,
    ocp_tripped = info.ocp,
    opp_tripped = info.opp,
  }
  result["function"] = query(ctx, "FUNC?", parse_function)
  result.input = query(ctx, "INP?", parse_bool)
  result.short = query(ctx, "INP:SHOR?", parse_bool)
  for name, setpoint in pairs(SETPOINTS) do
    result[name] = query(ctx, setpoint.command .. "?", tonumber)
  end
  return result
end

function set(ctx, name, value)
  if name == "function" then
    command(ctx, "FUNC " .. FUNCTIONS[value])
  elseif name == "input" then
    command(ctx, "INP " .. (value and "ON" or "OFF"))
  else
    -- In another function the load drops a setpoint without saying so. Switching for the user
    -- would change what it does to whatever is connected, so ask for it instead.
    local active = query(ctx, "FUNC?", parse_function)
    if active ~= name then
      error(name .. " is the setpoint of function " .. name .. ", but the load is in function "
        .. tostring(active) .. ": set the function too, e.g. `function " .. name .. " " .. name .. " <value>`")
    end
    local setpoint = SETPOINTS[name]
    command(ctx, setpoint.command .. " " .. string.format(setpoint.format, value))
  end
end
