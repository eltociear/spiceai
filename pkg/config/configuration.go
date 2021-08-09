package config

import (
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"time"

	"github.com/spf13/viper"
	"gopkg.in/yaml.v2"
)

type SpiceConfiguration struct {
	HttpPort    uint                      `json:"http_port,omitempty" mapstructure:"http_port,omitempty" yaml:"http_port,omitempty"`
	Connections map[string]ConnectionSpec `json:"connections,omitempty" yaml:"connections,omitempty"`
	Pods        []PodSpec                 `json:"pods,omitempty" yaml:"pods,omitempty"`
}

type ConnectionSpec struct {
	Name  string `json:"name,omitempty" yaml:"name,omitempty"`
	Token string `json:"token,omitempty" yaml:"token,omitempty"`
}

type PodSpec struct {
	Name   string      `json:"name,omitempty" yaml:"name,omitempty"`
	Models *ModelsSpec `json:"models,omitempty" yaml:"models,omitempty"`
}

type ModelsSpec struct {
	Downloader *GitHubModelDownloaderSpec `json:"downloader,omitempty" yaml:"downloader,omitempty"`
	Keep       uint                       `json:"keep,omitempty" yaml:"keep,omitempty"`
}

type GitHubModelDownloaderSpec struct {
	Uses   string  `json:"uses,omitempty" yaml:"uses,omitempty"`
	Branch *string `json:"branch,omitempty" yaml:"branch,omitempty"`
}

func LoadDefaultConfiguration() *SpiceConfiguration {
	return &SpiceConfiguration{
		HttpPort: 8000,
	}
}

func LoadRuntimeConfiguration(v *viper.Viper) (*SpiceConfiguration, error) {
	v.AddConfigPath(".spice")
	v.SetConfigName("config")
	v.SetConfigType("yaml")

	v.SetEnvPrefix("SPICE")
	v.AutomaticEnv()

	var config *SpiceConfiguration
	err := v.ReadInConfig()
	if err != nil {
		// No config file found, use defaults
		config = LoadDefaultConfiguration()
		spiceAppPath := AppSpicePath()
		configPath := filepath.Join(spiceAppPath, "config.yaml")
		marshalledConfig, err := yaml.Marshal(config)
		if err != nil {
			return nil, err
		}

		err = os.MkdirAll(spiceAppPath, 0766)
		if err != nil {
			return nil, fmt.Errorf("error initializing .spice/config.yaml: %w", err)
		}

		err = os.WriteFile(configPath, marshalledConfig, 0766)
		if err != nil {
			return nil, fmt.Errorf("error initializing .spice/config.yaml: %w", err)
		}

		// Wait for file flush to ensure viper.WatchConfig() works
		for i := 0; i < 10; i++ {
			_, err := os.Stat(configPath)
			if err != nil {
				break
			}
			time.Sleep(100 * time.Millisecond)
		}
		if err != nil {
			return nil, errors.New("error initializing .spice/config.yaml")
		}
	}

	v.WatchConfig()

	err = v.Unmarshal(&config)
	return config, err
}

func (rtConfig *SpiceConfiguration) ServerBaseUrl() string {
	return fmt.Sprintf("http://localhost:%d", rtConfig.HttpPort)
}
