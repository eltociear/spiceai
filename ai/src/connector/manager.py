from connector.openai_gym import OpenAIGymConnector
from connector.stateful import StatefulConnector
import pandas as pd
from enum import Enum
from typing import Union


class ConnectorType(Enum):
    STATEFUL = "stateful"
    OPENAI_GYM = "openai-gym"


ConnectorSpec = Union[StatefulConnector, OpenAIGymConnector]


class ConnectorManager:
    def __init__(self):
        self.connectors: "list[ConnectorSpec]" = []

    def add_connector(self, connector: ConnectorSpec):
        self.connectors.append(connector)

    def apply_action(self, action: int, data_row: pd.DataFrame) -> bool:
        is_valid = True
        for connector in self.connectors:
            if not connector.apply_action(action, data_row):
                is_valid = False
        return is_valid
