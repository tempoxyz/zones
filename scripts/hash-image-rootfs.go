// Hash a Docker-exported root filesystem without extracting untrusted files.
// Archive order and timestamps are ignored; contents and execution-relevant
// metadata are included in the digest.
package main

import (
	"archive/tar"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"io"
	"os"
	"sort"
)

type entry struct {
	Path     string            `json:"path"`
	Type     byte              `json:"type"`
	Mode     int64             `json:"mode"`
	UID      int               `json:"uid"`
	GID      int               `json:"gid"`
	Linkname string            `json:"linkname"`
	Devmajor int64             `json:"devmajor"`
	Devminor int64             `json:"devminor"`
	PAX      map[string]string `json:"pax"`
	Size     int64             `json:"size,omitempty"`
	SHA256   string            `json:"sha256,omitempty"`
}

func hashRootFS(input io.Reader) (string, error) {
	archive := tar.NewReader(input)
	var entries []entry
	seen := make(map[string]bool)
	for {
		header, err := archive.Next()
		if err == io.EOF {
			break
		}
		if err != nil {
			return "", err
		}
		if seen[header.Name] {
			return "", fmt.Errorf("duplicate path in Docker export: %q", header.Name)
		}
		seen[header.Name] = true

		pax := make(map[string]string)
		for key, value := range header.PAXRecords {
			switch key {
			case "mtime", "atime", "ctime", "path", "linkpath":
			default:
				pax[key] = value
			}
		}
		item := entry{
			Path: header.Name, Type: header.Typeflag, Mode: header.Mode,
			UID: header.Uid, GID: header.Gid, Linkname: header.Linkname,
			Devmajor: header.Devmajor, Devminor: header.Devminor, PAX: pax,
		}
		if header.Typeflag == tar.TypeReg || header.Typeflag == tar.TypeRegA {
			fileHash := sha256.New()
			if _, err := io.Copy(fileHash, archive); err != nil {
				return "", err
			}
			item.Size = header.Size
			item.SHA256 = hex.EncodeToString(fileHash.Sum(nil))
		}
		entries = append(entries, item)
	}

	sort.Slice(entries, func(i, j int) bool { return entries[i].Path < entries[j].Path })
	manifest, err := json.Marshal(entries)
	if err != nil {
		return "", err
	}
	digest := sha256.Sum256(manifest)
	return hex.EncodeToString(digest[:]), nil
}

func main() {
	digest, err := hashRootFS(os.Stdin)
	if err != nil {
		fmt.Fprintln(os.Stderr, "Unable to hash Docker export:", err)
		os.Exit(1)
	}
	fmt.Println(digest)
}
