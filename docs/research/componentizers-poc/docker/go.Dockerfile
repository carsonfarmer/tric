FROM golang:1.27.1-trixie
RUN go install github.com/bytecodealliance/componentize-go@v0.4.3 && componentize-go --version && componentize-go --help | head -30
