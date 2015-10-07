// Copyright 2015 The dpar Authors. All rights reserved.
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

package main

import (
	"bufio"
	"flag"
	"fmt"
	"hash/fnv"
	"log"
	"os"
	"time"

	"github.com/danieldk/conllx"
	"github.com/danieldk/dpar/cmd/common"
	"github.com/danieldk/dpar/features/symbolic"
	"github.com/danieldk/dpar/ml/svm"
	"github.com/danieldk/dpar/system"
	"gopkg.in/danieldk/golinear.v1"
)

func init() {
	flag.Usage = func() {
		fmt.Fprintf(os.Stderr, "Usage: %s [options] config input.conllx\n\n", os.Args[0])
		flag.PrintDefaults()
	}
}

func main() {
	flag.Parse()

	if flag.NArg() != 2 {
		flag.Usage()
		os.Exit(1)
	}

	configFile, err := os.Open(flag.Arg(0))
	common.ExitIfError(err)
	defer configFile.Close()
	config, err := common.ParseConfig(configFile)
	common.ExitIfError(err)

	generator, err := common.ReadFeatures(config.Parser.Features)
	common.ExitIfError(err)

	transitionSystem, ok := common.TransitionSystems[config.Parser.System]
	if !ok {
		log.Fatalf("Unknown transition system: %s", config.Parser.System)
	}

	labelNumberer, err := common.ReadTransitions(config.Parser.Transitions, transitionSystem)
	common.ExitIfError(err)

	model, err := golinear.LoadModel(config.Parser.Model)
	common.ExitIfError(err)

	hashKernelParsing(transitionSystem, generator, model, labelNumberer,
		config.Parser.HashKernelSize)
}

func hashKernelParsing(transitionSystem system.TransitionSystem,
	generator symbolic.FeatureGenerator, model *golinear.Model,
	labelNumberer *system.LabelNumberer, hashKernelSize uint) {
	guide := svm.NewHashingSVMGuide(model, generator, *labelNumberer, fnv.New32,
		hashKernelSize)
	parser := system.NewGreedyParser(transitionSystem, guide)

	start := time.Now()
	run(parser)
	elapsed := time.Since(start)
	log.Printf("Parsing took %s\n", elapsed)
}

func run(parser system.Parser) {
	inputFile, err := os.Open(flag.Arg(1))
	defer inputFile.Close()
	if err != nil {
		panic("Cannot open training data")
	}

	inputReader := conllx.NewReader(bufio.NewReader(inputFile))
	writer := conllx.NewWriter(os.Stdout)

	for {
		s, err := inputReader.ReadSentence()
		if err != nil {
			break
		}

		deps, err := parser.Parse(s)
		common.ExitIfError(err)

		// Clear to ensure that no dependencies in the input leak
		// (if they were present).
		for idx := range s {
			s[idx].SetHead(0)
			s[idx].SetHeadRel("NULL")
		}

		for dep := range deps {
			s[dep.Dependent-1].SetHead(dep.Head)
			s[dep.Dependent-1].SetHeadRel(dep.Relation)
		}

		writer.WriteSentence(s)
	}
}