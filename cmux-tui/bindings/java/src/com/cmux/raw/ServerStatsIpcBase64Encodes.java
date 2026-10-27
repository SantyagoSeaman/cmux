// Generated from cmux-tui/spec/sdk-schema.json. DO NOT EDIT.
package com.cmux.raw;


import java.util.ArrayList;
import java.util.Collections;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.Objects;


public final class ServerStatsIpcBase64Encodes implements WireValue {
    private final UInt64 frames;
    private final UInt64 lines;

    private ServerStatsIpcBase64Encodes(Builder builder) {
        if (!builder.framesSet) throw new IllegalArgumentException("frames is required");
        this.frames = Wire.nonNull(builder.frames, "frames");
        if (!builder.linesSet) throw new IllegalArgumentException("lines is required");
        this.lines = Wire.nonNull(builder.lines, "lines");
    }

    public static Builder builder() { return new Builder(); }

    public UInt64 frames() { return frames; }
    public UInt64 lines() { return lines; }

    public static ServerStatsIpcBase64Encodes fromWire(Object value) {
        Map<String, Object> object = Wire.object(value, "ServerStatsIpcBase64Encodes");
        Builder builder = builder();
        Object rawFrames = Wire.required(object, "frames");
        builder.frames(Wire.uint64(rawFrames, "ServerStatsIpcBase64Encodes.frames"));
        Object rawLines = Wire.required(object, "lines");
        builder.lines(Wire.uint64(rawLines, "ServerStatsIpcBase64Encodes.lines"));
        return builder.build();
    }

    @Override
    public Map<String, Object> toWire() {
        LinkedHashMap<String, Object> object = new LinkedHashMap<>();
        Wire.put(object, "frames", frames);
        Wire.put(object, "lines", lines);
        return Collections.unmodifiableMap(object);
    }

    @Override
    public boolean equals(Object other) {
        if (!(other instanceof ServerStatsIpcBase64Encodes that)) return false;
        return Objects.equals(frames, that.frames) && Objects.equals(lines, that.lines);
    }

    @Override
    public int hashCode() { return Objects.hash(frames, lines); }

    @Override
    public String toString() { return "ServerStatsIpcBase64Encodes" + toWire(); }

    public static final class Builder {
        private UInt64 frames;
        private boolean framesSet;
        private UInt64 lines;
        private boolean linesSet;

        public Builder frames(UInt64 value) {
            this.frames = value;
            this.framesSet = true;
            return this;
        }
        public Builder lines(UInt64 value) {
            this.lines = value;
            this.linesSet = true;
            return this;
        }
        public ServerStatsIpcBase64Encodes build() { return new ServerStatsIpcBase64Encodes(this); }
    }
}
