package symbolic

import (
	"bufio"
	"errors"
	"fmt"
	"hash"
	"io"
	"strings"

	"github.com/danieldk/dpar/features/addr"
	"github.com/danieldk/dpar/system"
)

// A funtion that produces a hash function.
type FeatureHashFun func() hash.Hash32

// A feature describes a part of a parser configuration. The
// // feature should be representable as a string or a hash.
type Feature interface {
	Hash(hf FeatureHashFun) uint32
	String() string
}

type FeatureSet map[string]float64

type FeatureVectorBuilder interface {
	Add(feature int, value float64)
}

// A feature generator generates concrete features based on a
// parser configuration. The feature set can be represented as
// (1) a string/value mapping or (2) a vector when feature hashing
// is used.
type FeatureGenerator interface {
	Generate(c *system.Configuration) FeatureSet
	GenerateHashed(c *system.Configuration, hf FeatureHashFun, fvb FeatureVectorBuilder)
}

// Functions that create a feature generators from a (possibly
// empty) list of arguments.
type FeatureGeneratorFactory func([]byte) (FeatureGenerator, error)

type FeatureGeneratorFactories map[string]FeatureGeneratorFactory

// An aggregate generator is a feature generator returns the
// set union of the output of the generators it wraps.
type AggregateGenerator struct {
	featureGenerators []FeatureGenerator
}

func NewAggregateGenerator(generators []FeatureGenerator) FeatureGenerator {
	return AggregateGenerator{generators}
}

func (a AggregateGenerator) Generate(c *system.Configuration) FeatureSet {
	combined := make(FeatureSet)

	for _, generator := range a.featureGenerators {
		for feature, value := range generator.Generate(c) {
			combined[feature] = value
		}
	}

	return combined
}

func (a AggregateGenerator) GenerateHashed(c *system.Configuration, hf FeatureHashFun,
	fvb FeatureVectorBuilder) {
	for _, generator := range a.featureGenerators {
		generator.GenerateHashed(c, hf, fvb)
	}
}

// Read feature descriptions with the default set of generators.
func ReadFeatureGeneratorsDefault(reader *bufio.Reader) (FeatureGenerator, error) {
	return ReadFeatureGenerators(FeatureGeneratorFactories{
		"addr": parseAddressedValueGenerator,
	}, reader)
}

func ReadFeatureGenerators(fs FeatureGeneratorFactories,
	reader *bufio.Reader) (FeatureGenerator, error) {
	var eof = false

	var generators []FeatureGenerator

	for !eof {
		line, err := reader.ReadString('\n')

		if err != nil {
			if err == io.EOF {
				eof = true
			} else {
				return nil, err
			}
		}

		line = strings.TrimSpace(line)

		if line == "" {
			continue
		}

		g, err := parseGenerator(fs, line)
		if err != nil {
			return nil, err
		}

		generators = append(generators, g)
	}

	return AggregateGenerator{generators}, nil
}

func parseGenerator(fs FeatureGeneratorFactories, line string) (FeatureGenerator, error) {
	sepIdx := strings.IndexByte(line, ' ')
	if sepIdx == -1 {
		return nil, errors.New("Line should at the very least specify a generator.")
	}

	generatorName := line[:sepIdx]
	factory, ok := fs[generatorName]
	if !ok {
		return nil, fmt.Errorf("Unknown generator: %s", generatorName)
	}

	return factory([]byte(line[sepIdx+1:]))
}

func parseAddressedValueGenerator(data []byte) (FeatureGenerator, error) {
	templates, err := addr.ParseAddressedValueTemplates(data)
	if err != nil {
		return nil, err
	}

	return NewAddressedValueGenerator(templates), nil
}
