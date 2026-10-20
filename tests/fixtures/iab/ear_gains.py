import json
import random
import sys

import numpy as np
from ear.core import bs2051
from ear.core.direct_speakers.panner import DirectSpeakersPanner
from ear.core.metadata_input import DirectSpeakersTypeMetadata, ExtraData, ObjectTypeMetadata
from ear.core.objectbased.decorrelate import design_decorrelators
from ear.core.objectbased.gain_calc import GainCalc
from ear.fileio.adm.elements import (
    AudioBlockFormatDirectSpeakers,
    AudioBlockFormatObjects,
    AudioPackFormat,
    BoundCoordinate,
    CartesianZone,
    ChannelLock,
    DirectSpeakerCartesianPosition,
    ObjectCartesianPosition,
    TypeDefinition,
)

LAYOUTS = ["0+2+0", "0+5+0", "0+7+0"]

# the nine IAB zones as ADM boxes (min X, max X, min Y, max Y, min Z, max Z),
# the same values as ZONE_BOXES in src/iab/panner.rs
EDGE = 0.01
COSINE_30 = 0.8660254037844386
COSINE_110 = -0.3420201433256687
ZONES = [
    (-1.0, -EDGE, COSINE_30, 1.0, -1.0, 0.0),
    (0.0, 0.0, COSINE_30, 1.0, -1.0, 0.0),
    (EDGE, 1.0, COSINE_30, 1.0, -1.0, 0.0),
    (-1.0, -EDGE, COSINE_110 + EDGE, COSINE_30 - EDGE, -1.0, 0.0),
    (EDGE, 1.0, COSINE_110 + EDGE, COSINE_30 - EDGE, -1.0, 0.0),
    (-1.0, -EDGE, -1.0, COSINE_110, -1.0, 0.0),
    (EDGE, 1.0, -1.0, COSINE_110, -1.0, 0.0),
    (-1.0, -EDGE, -1.0, 1.0, EDGE, 1.0),
    (EDGE, 1.0, -1.0, 1.0, EDGE, 1.0),
]

# SMPTE RDD 57 Table 4, the zones each set turns off (ZoneGainPrefix 0), counted from 0
RDD57_TABLE4_EXCLUDED = {
    "1a": [5, 6],
    "1b": [5, 6, 7, 8],
    "2a": [3, 4],
    "2b": [3, 4, 7, 8],
    "3a": [0, 2, 3, 4],
    "3b": [0, 2, 3, 4, 7, 8],
    "4a": [3, 4, 5, 6],
    "4b": [3, 4, 5, 6, 7, 8],
    "5a": [0, 1, 2],
    "5b": [0, 1, 2, 7, 8],
    "6": [7, 8],
}

FLOOR_POINTS = [
    (-1.0, 1.0, 0.0),
    (1.0, 1.0, 0.0),
    (-1.0, -1.0, 0.0),
    (1.0, -1.0, 0.0),
    (0.0, 1.0, 0.0),
    (0.0, -1.0, 0.0),
    (-1.0, 0.0, 0.0),
    (1.0, 0.0, 0.0),
    (0.0, 0.0, 0.0),
    (-0.5, 0.3, 0.0),
    (0.25, -0.75, 0.0),
    (0.8, 0.6, 0.0),
    (-0.3, -0.2, 0.0),
    (0.1, 0.9, 0.0),
    (-0.9, 0.5, 0.0),
]
CEILING_POINTS = [
    (-0.5, 0.3, 1.0),
    (0.0, 0.0, 1.0),
    (1.0, 1.0, 1.0),
    (-1.0, -1.0, 1.0),
    (0.4, -0.4, 0.5),
]
EXTENT_POINTS = [
    (0.0, 0.0, 0.0),
    (-0.5, 0.3, 0.0),
    (1.0, 1.0, 0.0),
    (-1.0, -0.6, 0.0),
    (0.3, 0.95, 0.5),
    (0.7, -0.9, 0.0),
]
EXTENTS = [0.02, 0.1, 0.25, 0.5, 0.75, 1.0]
# zone sets that leave the loudspeakers varying along fewer axes, such as L and Ls alone on 0+5+0
REDUCED_LAYOUT_ZONES = {
    "0+2+0": [[0], [2]],
    "0+5+0": [[1, 2, 4, 6], [0, 1, 3, 5], [0, 2]],
    "0+7+0": [[1, 2, 4, 6], [0, 1, 3, 5]],
}
REDUCED_LAYOUT_POINTS = [(-0.4, 0.2, 1.0), (0.3, -0.7, 0.0), (-1.0, 0.5, 0.5), (0.9, 0.9, 0.0)]
REDUCED_LAYOUT_EXTENTS = [0.1, 0.61, 1.0]
RANDOM_CASE_COUNT = 60
ZONE_POINTS = [
    (-1.0, 1.0, 0.0),
    (0.0, 1.0, 0.0),
    (1.0, 1.0, 0.0),
    (-1.0, 0.0, 0.0),
    (1.0, 0.0, 0.0),
    (-1.0, -1.0, 0.0),
    (1.0, -1.0, 0.0),
    (0.0, 0.0, 0.0),
    (-0.5, 0.3, 0.0),
]


def object_case(layout_name, calculator, position, extent, excluded_zones, snap, diffuse):
    block_format = AudioBlockFormatObjects(
        position=ObjectCartesianPosition(X=position[0], Y=position[1], Z=position[2]),
        cartesian=True,
        width=extent,
        height=extent,
        depth=extent,
        diffuse=diffuse,
        channelLock=ChannelLock() if snap else None,
        zoneExclusion=[
            CartesianZone(minX=box[0], maxX=box[1], minY=box[2], maxY=box[3], minZ=box[4], maxZ=box[5])
            for box in (ZONES[zone] for zone in excluded_zones)
        ],
    )
    gains = calculator.render(ObjectTypeMetadata(block_format=block_format, extra_data=ExtraData()))
    return {
        "layout": layout_name,
        "position": list(position),
        "extent": extent,
        "excluded_zones": list(excluded_zones),
        "snap": snap,
        "diffuse": diffuse,
        "direct_gains": gains.direct.tolist(),
        "diffuse_gains": gains.diffuse.tolist(),
    }


def object_cases(layout_name):
    calculator = GainCalc(bs2051.get_layout(layout_name))
    cases = []

    def add(position, extent=0.0, excluded_zones=(), snap=False, diffuse=0.0):
        cases.append(object_case(layout_name, calculator, position, extent, excluded_zones, snap, diffuse))

    for position in FLOOR_POINTS + CEILING_POINTS:
        add(position)
    for position in EXTENT_POINTS:
        for extent in EXTENTS:
            add(position, extent=extent)
    for zone in range(len(ZONES)):
        for position in ZONE_POINTS:
            add(position, excluded_zones=[zone])
    for excluded in RDD57_TABLE4_EXCLUDED.values():
        for position in ZONE_POINTS:
            add(position, excluded_zones=excluded)
        add((-0.5, 0.3, 0.0), extent=0.5, excluded_zones=excluded)
    for position in FLOOR_POINTS + CEILING_POINTS:
        add(position, snap=True)
    for zone in range(len(ZONES)):
        add((-0.6, 0.8, 0.0), snap=True, excluded_zones=[zone])
        add((0.0, 0.0, 0.0), snap=True, excluded_zones=[zone])
    add((-0.5, 0.3, 0.0), extent=0.25, snap=True)
    for position in [(0.0, 0.0, 0.0), (-0.5, 0.3, 0.0), (1.0, -1.0, 0.0)]:
        add(position, diffuse=1.0)
    add((-0.5, 0.3, 0.0), extent=0.5, diffuse=1.0, excluded_zones=[3])
    for excluded in REDUCED_LAYOUT_ZONES[layout_name]:
        for position in REDUCED_LAYOUT_POINTS:
            for extent in REDUCED_LAYOUT_EXTENTS:
                add(position, extent=extent, excluded_zones=excluded)
    generator = random.Random(layout_name)
    for _ in range(RANDOM_CASE_COUNT):
        position = (
            generator.uniform(-1.0, 1.0),
            generator.uniform(-1.0, 1.0),
            generator.choice([0.0, 1.0, generator.uniform(0.0, 1.0)]),
        )
        add(
            position,
            extent=generator.choice([0.0, generator.uniform(0.0, 1.0)]),
            excluded_zones=[zone for zone in range(len(ZONES)) if generator.random() < 0.25],
            snap=generator.random() < 0.2,
            diffuse=1.0 if generator.random() < 0.15 else 0.0,
        )
    return cases


# SMPTE ST 2098-2 Table 19 channels Profile 1 allows, with the BS.2051 label of
# the 0+7+0 loudspeaker each one drives and its ADM Cartesian position
BED_CHANNELS = [
    (0x0, "M+030", (-1.0, 1.0, 0.0)),
    (0x2, "M+000", (0.0, 1.0, 0.0)),
    (0x4, "M-030", (1.0, 1.0, 0.0)),
    (0x5, "M+090", (-1.0, 0.0, 0.0)),
    (0x7, "M+135", (-1.0, -1.0, 0.0)),
    (0x8, "M-135", (1.0, -1.0, 0.0)),
    (0x9, "M-090", (1.0, 0.0, 0.0)),
    (0xB, None, (-0.5, 0.0, 1.0)),
    (0xC, None, (0.5, 0.0, 1.0)),
    (0xD, "LFE1", (-1.0, 1.0, -1.0)),
]


def direct_speakers_cases(layout_name):
    panner = DirectSpeakersPanner(bs2051.get_layout(layout_name))
    pack_0_7_0 = AudioPackFormat(
        id="AP_0001000f",
        audioPackFormatName="RoomCentric_7.1",
        type=TypeDefinition.DirectSpeakers,
        is_common_definition=True,
    )
    cases = []
    for channel_id, label, position in BED_CHANNELS:
        block_format = AudioBlockFormatDirectSpeakers(
            position=DirectSpeakerCartesianPosition(
                bounded_X=BoundCoordinate(position[0]),
                bounded_Y=BoundCoordinate(position[1]),
                bounded_Z=BoundCoordinate(position[2]),
            ),
            speakerLabel=[label] if label else [],
        )
        metadata = DirectSpeakersTypeMetadata(
            block_format=block_format,
            audioPackFormats=[pack_0_7_0] if label else None,
            extra_data=ExtraData(),
        )
        cases.append(
            {"layout": layout_name, "channel_id": channel_id, "gains": panner.handle(metadata).tolist()}
        )
    return cases


def main():
    seven_one = bs2051.get_layout("0+7+0")
    reference = {
        "zones": [list(box) for box in ZONES],
        "layouts": {name: bs2051.get_layout(name).channel_names for name in LAYOUTS},
        "objects": [case for name in LAYOUTS for case in object_cases(name)],
        "direct_speakers": [case for name in LAYOUTS for case in direct_speakers_cases(name)],
        # one filter per 0+7+0 channel in layout order, transposed to filter by channel
        "decorrelation_channel_names": seven_one.channel_names,
        "decorrelation_filters": np.asarray(design_decorrelators(seven_one)).T.tolist(),
    }
    json.dump(reference, sys.stdout, indent=None, separators=(",", ":"))
    sys.stdout.write("\n")


main()
