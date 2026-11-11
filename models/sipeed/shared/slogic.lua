-- Capture driver for Sipeed's SLogic logic analyzers, through Sipeed's sigrok build (sigrok
-- driver `sipeed-slogic-analyzer`): the `capture` action into a file, and the `samples` stream.
-- Models differ only in their rate tables (PARAMS).
--
-- The vendor driver doesn't refuse what the analyzer can't do, it only warns and captures
-- something else: a sample rate it doesn't have becomes the highest of the channel mode, a rate
-- above the mode's limit is cut down to it, an unknown channel mode becomes all channels. So
-- everything is checked here first, and a capture only runs as it was asked for.

PARAMS = {
  samplerates    = { type = "array", required = true, doc = "the sample rates the analyzer has, in Hz" },
  max_samplerate = { type = "table", required = true, doc = "channel mode (number of channels) -> its highest sample rate in Hz, on the fastest USB link" },
}

-- sigrok's output module -> the file's extension.
local EXTENSIONS = { srzip = "sr", binary = "bin", csv = "csv", vcd = "vcd" }

-- The argument's choices -> the vendor driver's names (`desk sigrok show`).
local PATTERNS = { normal = "Normal", emulation = "Emulation" }

-- sigrok-cli keeps working after the capture itself: it writes the file while samples arrive
-- faster than it can convert them. Channel-samples per second it is given for that, per output
-- module: about a tenth of what a desktop PC managed (../README.md, *Observed behavior*).
local PACE = { srzip = 8e6, binary = 100e6, csv = 0.2e6, vcd = 1e6 }

local function mhz(rate)
  return string.format("%g MHz", rate / 1e6)
end

local function rates_up_to(rates, limit)
  local out = {}
  for _, r in ipairs(rates) do
    if r <= limit then
      out[#out + 1] = string.format("%g", r / 1e6)
    end
  end
  return table.concat(out, ", ") .. " MHz"
end

-- Bytes per sample in sigrok's `binary` output, per channel mode. Measured on the SLogic32U3
-- (../README.md, *Observed behavior*).
local UNIT_SIZE = { ["4"] = 1, ["8"] = 1, ["16"] = 2, ["32"] = 4 }

-- Checks what the capture and the stream share, and returns the channel count and sigrok's
-- config keys. desk writes the keys sorted, so `logic_channels` comes before `samplerate`: the
-- vendor driver needs that order, since the channel mode decides which rates are allowed.
local function setup(ctx, args)
  local limit = ctx.params.max_samplerate[args.channels]
  if not limit then
    error("the analyzer has no mode with " .. args.channels .. " channels")
  end
  local known = false
  for _, r in ipairs(ctx.params.samplerates) do
    known = known or r == args.samplerate
  end
  if not known or args.samplerate > limit then
    error(string.format("no samplerate %s with %s channels: it has %s (fewer channels allow more)",
      mhz(args.samplerate), args.channels, rates_up_to(ctx.params.samplerates, limit)))
  end
  -- One switching point for all inputs, in 0.1 V steps; sigrok wants it as a low-high pair.
  local tenths = math.floor(args.threshold * 10 + 0.5)
  if math.abs(args.threshold * 10 - tenths) > 1e-6 then
    error("the threshold has 0.1 V steps")
  end
  local threshold = string.format("%.1f", tenths / 10)
  local channels = math.tointeger(tonumber(args.channels))
  return channels, tenths / 10, {
    logic_channels    = channels,
    pattern           = PATTERNS[args.pattern],
    samplerate        = args.samplerate,
    voltage_threshold = threshold .. "-" .. threshold,
  }
end

function action(ctx, name, args)
  if name ~= "capture" then
    error("unknown action " .. name)
  end
  local channels, threshold, config = setup(ctx, args)
  local ms = math.ceil(args.duration * 1000)
  local samples = ms / 1000 * args.samplerate
  local r = ctx.sigrok.acquire{
    config       = config,
    time         = ms,
    triggers     = args.trigger,
    wait_trigger = args.trigger ~= nil,
    format       = args.format,
    into         = ctx.output,
    -- The capture, writing the file, starting sigrok, and time for a trigger to come.
    timeout      = ms + math.ceil(samples * channels / PACE[args.format] * 1000)
                   + (args.trigger and 30000 or 10000),
  }
  return {
    format     = args.format,
    extension  = EXTENSIONS[args.format],
    samplerate = args.samplerate,
    channels   = channels,
    duration   = ms / 1000,
    threshold  = threshold,
    pattern    = args.pattern,
    trigger    = args.trigger,
    bytes      = r.bytes,
  }
end

-- `samples`: sigrok's `binary` output from its stdout, until the stream is stopped. All channels
-- of the mode, D0 in bit 0.
function stream(ctx, name, args)
  if name ~= "samples" then
    error("unknown stream " .. name)
  end
  local channels, threshold, config = setup(ctx, args)
  local names = {}
  for i = 0, channels - 1 do
    names[#names + 1] = "D" .. i
  end
  ctx.describe{
    into       = ctx.output,
    unit_size  = UNIT_SIZE[args.channels],
    samplerate = args.samplerate,
    channels   = names,
    threshold  = threshold,
    pattern    = args.pattern,
  }
  ctx.sigrok.acquire{
    config     = config,
    continuous = true,
    format     = "binary",
    into       = ctx.output,
  }
end
