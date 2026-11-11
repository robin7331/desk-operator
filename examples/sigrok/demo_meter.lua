-- A meter driver for stock sigrok's `demo` device: reads its analog channels with
-- ctx.sigrok.measure and a config key of channel group A0 with ctx.sigrok.get.

PARAMS = {}

function read(ctx)
  local out = {}
  -- One sample per channel. Values arrive in the base unit (a printed `mV` becomes V).
  for _, r in ipairs(ctx.sigrok.measure{ channels = { "A0", "A1" }, samples = 1 }) do
    if r.value ~= nil and r.unit == "V" then
      out[string.lower(r.channel)] = r.value
    end
  end
  local g = ctx.sigrok.get{ keys = { "amplitude" }, group = "A0" }
  out.amplitude = g.amplitude
  return out
end
