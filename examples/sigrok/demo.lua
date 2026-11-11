-- A capture driver for stock sigrok's `demo` device: maps the action's arguments to
-- ctx.sigrok.acquire and describes the file it produced. Its stream `samples` delivers the logic
-- channels live (`desk stream la samples`).

PARAMS = {}

-- sigrok's output module -> the file's extension.
local EXTENSIONS = { srzip = "sr", binary = "bin", csv = "csv", vcd = "vcd" }

function action(ctx, name, args)
  if name ~= "capture" then
    error("unknown action " .. name)
  end
  local ms = math.ceil(args.duration * 1000)
  local r = ctx.sigrok.acquire{
    config       = { samplerate = args.samplerate },
    channels     = args.channels,
    time         = ms,
    triggers     = args.trigger,
    wait_trigger = args.trigger ~= nil,
    format       = args.format,
    into         = ctx.output,
    -- The capture, plus time to start sigrok and to wait for a trigger.
    timeout      = ms + (args.trigger and 30000 or 10000),
  }
  return {
    format     = args.format,
    extension  = EXTENSIONS[args.format],
    samplerate = args.samplerate,
    channels   = args.channels,
    duration   = args.duration,
    trigger    = args.trigger,
    bytes      = r.bytes,
  }
end

-- The channel names of "D0,D1" or "D0-D3" in the order sigrok packs them into a sample: the
-- first enabled channel is bit 0.
local function channel_list(text)
  local out = {}
  for part in string.gmatch(text, "[^,]+") do
    part = part:match("^%s*(.-)%s*$")
    local from, to = part:match("^D(%d+)%-D(%d+)$")
    if from then
      for i = tonumber(from), tonumber(to) do out[#out + 1] = "D" .. i end
    else
      out[#out + 1] = part
    end
  end
  return out
end

function stream(ctx, name, args)
  if name ~= "samples" then
    error("unknown stream " .. name)
  end
  local channels = channel_list(args.channels)
  -- sigrok's binary output: one byte per sample for up to 8 channels (the demo has 8).
  if #channels > 8 then
    error("the demo device has 8 logic channels")
  end
  ctx.describe{
    into       = ctx.output,
    unit_size  = 1,
    samplerate = args.samplerate,
    channels   = channels,
    driver     = ctx.sigrok.driver,
  }
  ctx.sigrok.acquire{
    config     = { samplerate = args.samplerate },
    channels   = channels,
    continuous = true,
    format     = "binary",
    into       = ctx.output,
  }
end
