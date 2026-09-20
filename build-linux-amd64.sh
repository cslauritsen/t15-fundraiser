#!/bin/bash


cd $(dirname $0)

docker build \
	--platform linux/amd64 \
	-e GIT_DESCRIBE=$(git describe --dirty --tags --always) \
	-t cslauritsen/t15-fundraiser:${TAG:-latest} \
	.

