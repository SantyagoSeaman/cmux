// Generated from cmux-tui/spec/sdk-schema.json. DO NOT EDIT.
package com.cmux.raw;


import java.util.ArrayList;
import java.util.Collections;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.Objects;


public final class ServerStatsIpcFrameBreaker implements WireValue {
    private final UInt64 framedBadMessage;
    private final boolean tripped;

    private ServerStatsIpcFrameBreaker(Builder builder) {
        if (!builder.framedBadMessageSet) throw new IllegalArgumentException("framed_bad_message is required");
        this.framedBadMessage = Wire.nonNull(builder.framedBadMessage, "framed_bad_message");
        if (!builder.trippedSet) throw new IllegalArgumentException("tripped is required");
        this.tripped = builder.tripped;
    }

    public static Builder builder() { return new Builder(); }

    public UInt64 framedBadMessage() { return framedBadMessage; }
    public boolean tripped() { return tripped; }

    public static ServerStatsIpcFrameBreaker fromWire(Object value) {
        Map<String, Object> object = Wire.object(value, "ServerStatsIpcFrameBreaker");
        Builder builder = builder();
        Object rawFramedBadMessage = Wire.required(object, "framed_bad_message");
        builder.framedBadMessage(Wire.uint64(rawFramedBadMessage, "ServerStatsIpcFrameBreaker.framed_bad_message"));
        Object rawTripped = Wire.required(object, "tripped");
        builder.tripped(Wire.bool(rawTripped, "ServerStatsIpcFrameBreaker.tripped"));
        return builder.build();
    }

    @Override
    public Map<String, Object> toWire() {
        LinkedHashMap<String, Object> object = new LinkedHashMap<>();
        Wire.put(object, "framed_bad_message", framedBadMessage);
        Wire.put(object, "tripped", tripped);
        return Collections.unmodifiableMap(object);
    }

    @Override
    public boolean equals(Object other) {
        if (!(other instanceof ServerStatsIpcFrameBreaker that)) return false;
        return Objects.equals(framedBadMessage, that.framedBadMessage) && Objects.equals(tripped, that.tripped);
    }

    @Override
    public int hashCode() { return Objects.hash(framedBadMessage, tripped); }

    @Override
    public String toString() { return "ServerStatsIpcFrameBreaker" + toWire(); }

    public static final class Builder {
        private UInt64 framedBadMessage;
        private boolean framedBadMessageSet;
        private Boolean tripped;
        private boolean trippedSet;

        public Builder framedBadMessage(UInt64 value) {
            this.framedBadMessage = value;
            this.framedBadMessageSet = true;
            return this;
        }
        public Builder tripped(boolean value) {
            this.tripped = value;
            this.trippedSet = true;
            return this;
        }
        public ServerStatsIpcFrameBreaker build() { return new ServerStatsIpcFrameBreaker(this); }
    }
}
