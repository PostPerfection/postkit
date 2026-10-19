#include "common/IABElements.h"
#include "IABParserAPI.h"

#include <cinttypes>
#include <cstdio>
#include <fstream>
#include <map>
#include <string>
#include <vector>

using namespace SMPTE::ImmersiveAudioBitstream;

static uint64_t digest(const std::vector<int32_t>& samples)
{
    uint64_t hash = 0xcbf29ce484222325ULL;
    for (int32_t sample : samples)
    {
        uint32_t bits = static_cast<uint32_t>(sample);
        for (int byte = 0; byte < 4; byte++)
        {
            hash ^= (bits >> (8 * byte)) & 0xff;
            hash *= 0x100000001b3ULL;
        }
    }
    return hash;
}

static void printRejected(const char* streamName, uint32_t frameIndex)
{
    std::printf("%s %u rejected\n", streamName, frameIndex);
}

static void printFrame(const char* streamName, uint32_t frameIndex, const std::map<IABAudioDataIDType, std::vector<int32_t>>& decoded)
{
    for (const auto& entry : decoded)
    {
        std::printf("%s %u %u %zu %016" PRIx64 "\n", streamName, frameIndex, entry.first, entry.second.size(), digest(entry.second));
    }
}

int main(int argc, char** argv)
{
    if (argc != 3)
    {
        std::fprintf(stderr, "usage: %s <stream name> <file.iab>\n", argv[0]);
        return 2;
    }
    const char* streamName = argv[1];
    std::ifstream input(argv[2], std::ios::binary);
    if (!input)
    {
        std::fprintf(stderr, "cannot open %s\n", argv[2]);
        return 2;
    }
    IABParserInterface* parser = IABParserInterface::Create(&input);
    for (uint32_t frameIndex = 0;; frameIndex++)
    {
        iabError status = parser->ParseIABFrame();
        if (status == kIABParserEndOfStreamReached)
        {
            break;
        }
        if (status != kIABNoError)
        {
            printRejected(streamName, frameIndex);
            break;
        }
        const IABFrameInterface* frame = nullptr;
        parser->GetIABFrame(frame);
        IABSampleRateType sampleRate;
        frame->GetSampleRate(sampleRate);
        uint32_t sampleCount = parser->GetFrameSampleCount();

        std::map<IABAudioDataIDType, std::vector<int32_t>> decoded;
        bool rejected = false;
        std::vector<IABElement*> subElements;
        frame->GetSubElements(subElements);
        for (IABElement* element : subElements)
        {
            std::vector<int32_t> samples(sampleCount);
            IABAudioDataIDType audioDataId = 0;
            iabError decodeStatus = kIABNoError;
            if (IABAudioDataDLCInterface* dlc = dynamic_cast<IABAudioDataDLCInterface*>(element))
            {
                dlc->GetAudioDataID(audioDataId);
                decodeStatus = dlc->DecodeDLCToMonoPCM(samples.data(), sampleCount, sampleRate);
            }
            else if (IABAudioDataPCMInterface* pcm = dynamic_cast<IABAudioDataPCMInterface*>(element))
            {
                pcm->GetAudioDataID(audioDataId);
                decodeStatus = pcm->UnpackPCMToMonoSamples(samples.data(), sampleCount);
            }
            else
            {
                continue;
            }
            // an AudioDataID shared by two elements has no single decode to print
            if (decodeStatus != kIABNoError || !decoded.emplace(audioDataId, samples).second)
            {
                rejected = true;
                break;
            }
        }
        if (rejected)
        {
            printRejected(streamName, frameIndex);
            break;
        }
        printFrame(streamName, frameIndex, decoded);
    }
    IABParserInterface::Delete(parser);
    return 0;
}
