-- OWON XDM1041 bench multimeter: SCPI text over a USB serial adapter. Notes: ../README.md.
-- Reading sends queries only, so the front panel stays usable (no SYST:REM). Only the commands
-- below are ever sent: undocumented ones are dangerous (`INT:START` starts factory calibration).

PARAMS = {
  model     = { type = "string", default = "XDM1041", doc = "model name expected in *IDN?; refuse other devices" },
  baud      = { type = "integer", default = 115200, doc = "the meter's baud rate (Utility menu)" },
  wait      = { type = "string", default = "3s", doc = "how long to wait for the port to appear" },
  reply_ms  = { type = "number", default = 1000, doc = "wait for one reply" },
  attempts  = { type = "integer", default = 4, doc = "how often a query is sent before giving up" },
  gap_ms    = { type = "number", default = 200, doc = "quiet time after a command; replies to it are dropped" },
  settle_ms = { type = "number", default = 4000, doc = "wait after changing function or range: until then the meter reports false values (measured up to about 3.5 s at slow rate)" },
}

local VOLT_DC = { "50mV", "500mV", "5V", "50V", "500V", "1000V" }
local VOLT_AC = { "500mV", "5V", "50V", "500V", "750V" }
local AMPS = { "500uA", "5mA", "50mA", "500mA", "5A", "10A" }
local OHMS = { "500ohm", "5kohm", "50kohm", "500kohm", "5Mohm", "50Mohm" }
local FARADS = { "50nF", "500nF", "5uF", "50uF", "500uF", "5mF", "50mF" }

-- `ranges`: in the order of `RANGE <n>`, starting at 1.
local FUNCTIONS = {
  dc_voltage         = { conf = "CONF:VOLT:DC AUTO", unit = "V", ranges = VOLT_DC },
  ac_voltage         = { conf = "CONF:VOLT:AC AUTO", unit = "V", ranges = VOLT_AC },
  dc_current         = { conf = "CONF:CURR:DC AUTO", unit = "A", ranges = AMPS },
  ac_current         = { conf = "CONF:CURR:AC AUTO", unit = "A", ranges = AMPS },
  resistance         = { conf = "CONF:RES AUTO", unit = "Ω", ranges = OHMS },
  capacitance        = { conf = "CONF:CAP AUTO", unit = "F", ranges = FARADS },
  frequency          = { conf = "CONF:FREQ", unit = "Hz" },
  period             = { conf = "CONF:PER", unit = "s" },
  diode              = { conf = "CONF:DIOD", unit = "V" },
  continuity         = { conf = "CONF:CONT", unit = "Ω" },
  temperature_kits90 = { conf = "CONF:TEMP:RTD KITS90" },
  temperature_pt100  = { conf = "CONF:TEMP:RTD PT100" },
}

-- FUNC? replies. TEMP needs RANGE? to tell the sensor.
local FUNCTION_REPLIES = {
  ["VOLT"] = "dc_voltage",
  ["VOLT AC"] = "ac_voltage",
  ["CURR"] = "dc_current",
  ["CURR AC"] = "ac_current",
  ["RES"] = "resistance",
  ["CAP"] = "capacitance",
  ["FREQ"] = "frequency",
  ["PER"] = "period",
  ["DIOD"] = "diode",
  ["CONT"] = "continuity",
}

local RATE_COMMANDS = { slow = "S", medium = "M", fast = "F" }
-- The manual says L for slow, the meters tools talk to say S.
local RATE_REPLIES = { S = "slow", L = "slow", M = "medium", F = "fast" }

-- MEAS1? says 1E+9 for "no valid reading": over range or open input, in any function, and briefly
-- after a function change. No real reading of this meter comes close.
local OVERLOAD = 1e9

local port
local firmware
local swap_diode_continuity = false

local function trim(s)
  return (s:gsub("^[%c%s]+", ""):gsub("[%c%s]+$", ""))
end

-- Send a query until a reply passes `parse` (returning non-nil). nil if none does. Stray lines are
-- normal here: some firmware answers commands with OK lines, and replies get lost or doubled. The
-- first reply after the meter wakes up starts with the bytes 00 01 00: `trim` removes them.
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
  port:clear()
end

function open(ctx)
  port = ctx.serial.open { baud = ctx.params.baud, wait = ctx.params.wait }
  local idn = query(ctx, "*IDN?", function(line)
    return line:match("^OWON,") and line or nil
  end)
  if not idn then
    error("no answer to *IDN? from an OWON meter: in standby it doesn't answer, press its front power button (or still booting, wrong port, wrong baud rate)")
  end
  local model, version = idn:match("^OWON,([^,]*),[^,]*,([^,]*)")
  if model ~= ctx.params.model then
    error("expected an OWON " .. ctx.params.model .. ", found: " .. idn)
  end
  firmware = version
  -- Below V4.3.0, FUNC? mixes up diode and continuity.
  local major, minor = (version or ""):match("^V(%d+)%.(%d+)")
  major, minor = tonumber(major), tonumber(minor)
  swap_diode_continuity = major ~= nil and (major < 4 or (major == 4 and minor < 3))
end

local function normalize_range(raw)
  local s = raw:gsub('"', ""):gsub("%s", "")
  s = s:gsub("\xa6\xb8", "ohm"):gsub("\xce\xa9", "ohm"):gsub("\xe2\x84\xa6", "ohm")
  s = s:gsub("\xaa\xcc", "u"):gsub("\xa6\xcc", "u"):gsub("\xce\xbc", "u"):gsub("\xc2\xb5", "u")
  return s
end

local function read_active_range(ctx)
  return query(ctx, "RANGE?", function(line)
    local range = normalize_range(line)
    local sensor = range:upper()
    if sensor == "KITS90" or sensor == "PT100" or range:match("^%d+%a+$") then
      return range
    end
    return nil
  end)
end

local function read_function(ctx)
  local name = query(ctx, "FUNC?", function(line)
    return FUNCTION_REPLIES[line:gsub('"', "")] or (line:gsub('"', "") == "TEMP" and "TEMP") or nil
  end)
  if not name then
    error("no valid answer to FUNC?")
  end
  if swap_diode_continuity then
    if name == "diode" then
      name = "continuity"
    elseif name == "continuity" then
      name = "diode"
    end
  end
  if name == "TEMP" then
    local sensor = read_active_range(ctx)
    if sensor and sensor:upper() == "KITS90" then
      return "temperature_kits90"
    elseif sensor and sensor:upper() == "PT100" then
      return "temperature_pt100"
    end
    error("unknown temperature sensor: " .. tostring(sensor))
  end
  return name
end

-- A range name of `ranges` matching what RANGE? says (the meter writes e.g. 5KΩ for 5kohm).
local function match_range(ranges, active)
  for _, name in ipairs(ranges) do
    if active and name:lower() == active:lower() then
      return name
    end
  end
  return nil
end

-- The temperature unit is only visible in the formatted reading.
local function read_temperature_unit(ctx)
  return query(ctx, "MEAS1:SHOW?", function(line)
    if line:find("\xa1\xe6", 1, true) or line:find("\xc2\xb0C", 1, true) then
      return "°C"
    elseif line:find("\xa8\x48", 1, true) or line:find("\xc2\xb0F", 1, true) then
      return "°F"
    elseif line:match("%d%s*K$") then
      return "K"
    end
    return nil
  end)
end

function read(ctx)
  local result = { firmware = firmware }
  local name = read_function(ctx)
  local fn = FUNCTIONS[name]
  result["function"] = name

  if fn.ranges then
    local auto = query(ctx, "AUTO?", function(line)
      return line == "1" and "auto" or line == "0" and "manual" or nil
    end)
    local active = read_active_range(ctx)
    result.active_range = match_range(fn.ranges, active) or active
    if auto == "auto" then
      result.range = "auto"
    elseif auto == "manual" then
      result.range = match_range(fn.ranges, active)
    end
  end

  result.rate = query(ctx, "RATE?", function(line)
    return RATE_REPLIES[line:gsub('"', "")]
  end)

  local value = query(ctx, "MEAS1?", function(line)
    return tonumber(line)
  end)
  if value == nil then
    error("no valid answer to MEAS1?")
  end

  result.overload = value >= OVERLOAD
  if not result.overload then
    local unit = fn.unit or read_temperature_unit(ctx)
    if unit then
      result.reading = { value = value, unit = unit }
    end
  end
  return result
end

function set(ctx, name, value)
  if name == "function" then
    command(ctx, FUNCTIONS[value].conf)
    ctx.sleep(ctx.params.settle_ms)
  elseif name == "range" then
    local current = read_function(ctx)
    local ranges = FUNCTIONS[current].ranges
    if not ranges then
      error(current .. " has no ranges")
    end
    if value == "auto" then
      command(ctx, "AUTO")
    else
      local number
      for i, range in ipairs(ranges) do
        if range == value then
          number = i
        end
      end
      if not number then
        error(value .. " is not a range of " .. current .. ": " .. table.concat(ranges, ", "))
      end
      command(ctx, "RANGE " .. number)
    end
    ctx.sleep(ctx.params.settle_ms)
  elseif name == "rate" then
    command(ctx, "RATE " .. RATE_COMMANDS[value])
  end
end
